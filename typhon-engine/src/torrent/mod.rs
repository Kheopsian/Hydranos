pub mod meta;
pub mod metainfo;
pub mod piece_picker;
pub mod fastresume;
pub mod rate;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use tracing::{info, warn};

use meta::{InfoHash, TorrentState, TorrentStatus};
use crate::disk::DiskManager;
use std::sync::OnceLock;
use tokio::sync::Semaphore;
use sha1::{Sha1, Digest};

pub struct TorrentManager {
    torrents: DashMap<InfoHash, Arc<TorrentState>>,
    data_dir: String,
    resume_dir: String,
    disk: Arc<DiskManager>,
    pub upload_rate: rate::RateTracker,
    pub download_rate: rate::RateTracker,
    pub cached_unseeded_peers: std::sync::atomic::AtomicUsize,
    // O(1) MSE inbound resolution: SHA1("req2"+info_hash) -> info_hash.
    // Avoids the O(N) SHA1 scan over all torrents per inbound handshake.
    skey_index: DashMap<[u8; 20], InfoHash>,
}

impl TorrentManager {
    pub fn new(data_dir: String, resume_dir: String, disk: Arc<DiskManager>) -> Self {
        std::fs::create_dir_all(&resume_dir).ok();
        Self {
            torrents: DashMap::new(),
            data_dir,
            resume_dir,
            disk,
            upload_rate: rate::RateTracker::new(),
            download_rate: rate::RateTracker::new(),
            cached_unseeded_peers: std::sync::atomic::AtomicUsize::new(0),
            skey_index: DashMap::new(),
        }
    }

    pub fn get(&self, info_hash: &InfoHash) -> Option<Arc<TorrentState>> {
        self.torrents.get(info_hash).map(|r| r.value().clone())
    }

    pub fn has(&self, info_hash: &InfoHash) -> bool {
        self.torrents.contains_key(info_hash)
    }

    pub fn count(&self) -> usize {
        self.torrents.len()
    }

    pub fn all(&self) -> Vec<Arc<TorrentState>> {
        self.torrents.iter().map(|r| r.value().clone()).collect()
    }

    /// O(1) MSE SKEY resolution for an inbound handshake.
    pub fn lookup_skey(&self, req2_hash: &[u8; 20]) -> Option<InfoHash> {
        self.skey_index.get(req2_hash).map(|r| *r.value())
    }

    pub fn add_torrent(
        &self,
        torrent_path: &str,
        save_path: &str,
        stopped: bool,
        seed_mode: bool,
    ) -> Result<(InfoHash, String), String> {
        let meta = metainfo::parse_torrent_file(torrent_path)?;
        let ih = meta.info_hash;
        let name = meta.name.clone();

        if self.torrents.contains_key(&ih) {
            return Err("torrent already added".into());
        }

        let state = TorrentState::new(
            meta,
            save_path.into(),
            torrent_path.to_string(),
            seed_mode,
        );

        if stopped {
            state.status.store(TorrentStatus::Stopped as u8, Ordering::Relaxed);
            state.is_paused.store(true, Ordering::Relaxed);
        } else if !seed_mode {
            // Default-start fresh DL torrents in Downloading so the public
            // API doesn't surface "stopped" between add_torrent and the
            // first start_torrent call. Previously status stayed at 0.
            state.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        }

        // Save resume data
        let rd = fastresume::ResumeData {
            info_hash: hex_encode(&ih),
            torrent_path: torrent_path.to_string(),
            save_path: save_path.to_string(),
            seed_mode,
            paused: stopped,
            total_uploaded: 0,
            total_downloaded: 0,
            added_time: state.added_time,
            completed_time: state.completed_time.load(Ordering::Relaxed),
            bitfield: String::new(),
        };
        fastresume::save(&self.resume_dir, &ih, &rd);

        let state_arc = Arc::new(state);
        let total_size = state_arc.meta.total_size;
        let num_pieces = state_arc.meta.num_pieces();
        let private = state_arc.meta.private;
        self.skey_index.insert(crate::crypto::mse::sha1_combine(b"req2", &ih), ih);
        self.torrents.insert(ih, state_arc.clone());
        // Track in DHT (no-op for private torrents, see dht::track_torrent).
        crate::dht::track_torrent(state_arc);
        // Push event to subscribers (Go hydra cache). Silent if no subscribers.
        crate::rpc::events::publish(crate::rpc::events::Event::TorrentAdded {
            info_hash: hex_encode(&ih),
            name: name.clone(),
            save_path: save_path.to_string(),
            total_size,
            num_pieces,
            private,
            seed_mode,
        });
        Ok((ih, name))
    }

    pub fn remove_torrent(&self, info_hash: &InfoHash, keep_data: bool) -> Result<(), String> {
        fastresume::remove(&self.resume_dir, info_hash);
        // Flag the TorrentState as removed BEFORE dropping the DashMap entry
        // so in-flight peer tasks (which hold Arc<TorrentState>) can observe
        // the flag on their next loop iteration and exit cleanly. Otherwise
        // they keep servicing peers and write_piece recreates deleted files.
        //
        // When keep_data=false, capture the per-file paths now so we can
        // delete them after dropping the entry. We delete file-by-file (not
        // a recursive blast on the parent dir) so a torrent that happens to
        // share its parent directory with unrelated files cannot collateral-
        // damage them: only files this torrent owns are touched.
        let to_delete: Option<(Vec<std::path::PathBuf>, Option<std::path::PathBuf>)> =
            if let Some(t) = self.torrents.get(info_hash) {
                t.is_removed.store(true, Ordering::Relaxed);
                if !keep_data {
                    let files: Vec<std::path::PathBuf> = t.meta.files.iter().map(|f| {
                        if t.meta.multi_file {
                            t.save_path.read().join(&t.meta.name).join(&f.path)
                        } else {
                            t.save_path.read().join(&f.path)
                        }
                    }).collect();
                    let folder = if t.meta.multi_file {
                        Some(t.save_path.read().join(&t.meta.name))
                    } else {
                        None
                    };
                    Some((files, folder))
                } else { None }
            } else { None };
        self.skey_index.remove(&crate::crypto::mse::sha1_combine(b"req2", info_hash));
        let result = self.torrents.remove(info_hash)
            .map(|_| ())
            .ok_or_else(|| "torrent not found".into());
        if result.is_ok() {
            crate::rpc::events::publish(crate::rpc::events::Event::TorrentRemoved {
                info_hash: hex_encode(info_hash),
            });
            if let Some((files, folder)) = to_delete {
                for f in &files {
                    if let Err(e) = std::fs::remove_file(f) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            warn!("remove_torrent: failed to delete file {}: {}", f.display(), e);
                        }
                    }
                }
                // Drop cached fds for the just-unlinked files so the kernel
                // frees their blocks now (else /race leaks: the fd cache pins
                // deleted inodes until LRU eviction, which ~never happens).
                crate::disk::evict_fds(&files);
                // Multi-file: walk the torrent folder and remove empty subdirs
                // bottom-up. remove_dir() only succeeds when empty, so any
                // foreign file in there keeps its containing dir alive.
                if let Some(folder) = folder {
                    remove_empty_dirs_recursive(&folder);
                }
            }
        }
        result
    }

    pub fn start_torrent(&self, info_hash: &InfoHash) -> Result<(), String> {
        let t = self.get(info_hash).ok_or("torrent not found")?;
        t.is_paused.store(false, Ordering::Relaxed);
        // A recheck in progress owns the status. Don't let a start (e.g. from the
        // download slot manager filling a slot, or a resume) clobber Checking
        // with Downloading: that flips is_downloading() on and the torrent
        // re-downloads pieces the recheck is about to mark present (the "re-add
        // of already-complete data pulls 100-200 MB" bug). run_recheck sets the
        // final Seeding/Downloading itself when it finishes.
        if t.status.load(Ordering::Relaxed) == TorrentStatus::Checking as u8 {
            return Ok(());
        }
        if t.seed_mode || t.picker.get().is_none() {
            t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        } else {
            // Has a picker = was added without seed_mode = download mode
            let picker = t.picker.get().unwrap();
            let p = picker.lock().unwrap();
            if p.is_complete() {
                t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
            } else {
                t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    pub fn stop_torrent(&self, info_hash: &InfoHash) -> Result<(), String> {
        let t = self.get(info_hash).ok_or("torrent not found")?;
        t.is_paused.store(true, Ordering::Relaxed);
        t.status.store(TorrentStatus::Stopped as u8, Ordering::Relaxed);
        Ok(())
    }

    /// Suspend/resume disk serving for one torrent without pausing it: the
    /// torrent keeps its peer connections and keeps announcing (seedtime
    /// preserved), but serves no Piece Requests so it does zero disk I/O.
    /// Used by the per-disk seed-slot manager for HDD anti-thrash.
    pub fn set_serving_suspended(&self, info_hash: &InfoHash, suspended: bool) -> Result<(), String> {
        let t = self.get(info_hash).ok_or("torrent not found")?;
        t.serving_suspended.store(suspended, Ordering::Relaxed);
        Ok(())
    }

    pub fn load_resume_data(&self) -> usize {
        let resumes = fastresume::load_all(&self.resume_dir);
        let mut loaded = 0;
        for rd in resumes {
            let meta = match metainfo::parse_torrent_file(&rd.torrent_path) {
                Ok(m) => m,
                Err(e) => {
                    warn!("[resume] skip {}: {}", &rd.info_hash[..8.min(rd.info_hash.len())], e);
                    continue;
                }
            };
            let ih = meta.info_hash;
            if self.torrents.contains_key(&ih) {
                continue;
            }
            // Use new_with_times so added_time / completed_time persist across
            // reboots. Plain new() falls back to SystemTime::now(), which wipes
            // the fastresume history on every restart.
            let state = TorrentState::new_with_times(
                meta,
                rd.save_path.into(),
                rd.torrent_path,
                rd.seed_mode,
                Some(rd.added_time),
                Some(rd.completed_time),
            );
            state.total_uploaded.store(rd.total_uploaded, Ordering::Relaxed);
            state.total_downloaded.store(rd.total_downloaded, Ordering::Relaxed);
            // Restore verified-pieces bitfield FIRST so we don't re-DL 6+ GB
            // on every restart AND so an already-complete torrent is
            // recognised as a seed below. Pre-bitfield resume files have an
            // empty string here (serde default) — old behaviour preserved.
            if !rd.bitfield.is_empty() {
                if let Some(picker) = state.picker.get() {
                    let bytes = hex_decode_bytes(&rd.bitfield);
                    if !bytes.is_empty() {
                        picker.lock().unwrap().import_bitfield(&bytes);
                    }
                }
            }
            // Derive initial status from completion (same rule as
            // start_torrent) instead of hardcoding Downloading. Hardcoding
            // made every already-complete torrent load as Downloading then get
            // promoted Downloading->Seeding by a later runtime pass — a full
            // re-processing of the whole hoard on every restart (costly at tens
            // of thousands of torrents) plus a brief non-seeding window.
            if rd.paused {
                state.is_paused.store(true, Ordering::Relaxed);
                state.status.store(TorrentStatus::Stopped as u8, Ordering::Relaxed);
            } else if rd.seed_mode || state.picker.get().is_none() {
                state.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
            } else if state.picker.get().unwrap().lock().unwrap().is_complete() {
                state.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
            } else {
                state.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
            }
            self.skey_index.insert(crate::crypto::mse::sha1_combine(b"req2", &ih), ih);
            self.torrents.insert(ih, Arc::new(state));
            loaded += 1;
        }
        loaded
    }

    /// Update rate counters — call every 2s.
    pub fn update_rates(&self) {
        let mut total_ul = 0u64;
        let mut total_dl = 0u64;
        for entry in self.torrents.iter() {
            let t = entry.value();
            let ul = t.total_uploaded.load(Ordering::Relaxed);
            let dl = t.total_downloaded.load(Ordering::Relaxed);
            total_ul += ul;
            total_dl += dl;
            // Cold torrents (no peers, rate already 0) moved no bytes since the
            // last tick -> skip the EMA update so per-tick cost tracks the hot
            // set, not total N. One-tick under-report on wake is harmless.
            if t.peers_connected.load(Ordering::Relaxed) == 0
                && t.upload_rate.get() == 0
                && t.download_rate.get() == 0
            {
                continue;
            }
            t.upload_rate.update(ul);
            t.download_rate.update(dl);
        }
        self.upload_rate.update(total_ul);
        self.download_rate.update(total_dl);
    }

    // NOTE: per-peer rate tracking removed — on-demand compute done in
    // get_peers RPC handler directly from PeerStats atomics.

    /// Background: count unseeded peers across all torrents. Call every 30s.
    pub fn update_unseeded_count(&self) {
        let mut total = 0usize;
        for entry in self.torrents.iter() {
            total += entry.value().peer_stats.iter()
                .filter(|e| !e.value().is_seed.load(Ordering::Relaxed))
                .count();
        }
        self.cached_unseeded_peers.store(total, Ordering::Relaxed);
    }

    /// Persist current upload/download stats for all torrents.
    pub fn save_all_resume(&self) {
        for entry in self.torrents.iter() {
            let t = entry.value();
            let rd = Self::build_resume_data(t);
            fastresume::save(&self.resume_dir, &t.info_hash, &rd);
        }
    }

    /// Build a fastresume snapshot from the live TorrentState. Shared
    /// between save_all_resume (periodic) and set_save_path (immediate
    /// flush after a category move).
    fn build_resume_data(t: &TorrentState) -> fastresume::ResumeData {
        let bitfield = match t.picker.get() {
            Some(p) => hex_encode_bytes(&p.lock().unwrap().export_bitfield()),
            None => String::new(),
        };
        fastresume::ResumeData {
            info_hash: hex_encode(&t.info_hash),
            torrent_path: t.torrent_file_path.clone(),
            save_path: t.save_path.read().to_string_lossy().to_string(),
            seed_mode: t.seed_mode,
            paused: t.is_paused.load(Ordering::Relaxed),
            total_uploaded: t.total_uploaded.load(Ordering::Relaxed),
            total_downloaded: t.total_downloaded.load(Ordering::Relaxed),
            added_time: t.added_time,
            completed_time: t.completed_time.load(Ordering::Relaxed),
            bitfield,
        }
    }

    /// Swap the in-memory save_path for a running torrent and flush
    /// fastresume so the change survives a crash before the next
    /// periodic save. Caller (hydra-go) must have stopped the torrent
    /// and moved the files on disk *before* invoking this.
    pub fn set_save_path(&self, info_hash: &InfoHash, new_path: &str) -> Result<(), String> {
        let t = self.get(info_hash).ok_or("torrent not found")?;
        *t.save_path.write() = new_path.into();
        let rd = Self::build_resume_data(&t);
        fastresume::save(&self.resume_dir, info_hash, &rd);
        Ok(())
    }
}

pub fn hex_encode(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn hex_decode(hex: &str) -> Result<InfoHash, String> {
    if hex.len() != 40 {
        return Err("info_hash must be 40 hex chars".into());
    }
    let mut hash = [0u8; 20];
    for i in 0..20 {
        hash[i] = u8::from_str_radix(&hex[i*2..i*2+2], 16)
            .map_err(|_| "invalid hex")?;
    }
    Ok(hash)
}

fn hex_encode_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode_bytes(hex: &str) -> Vec<u8> {
    if hex.len() % 2 != 0 { return Vec::new(); }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        match u8::from_str_radix(&hex[i..i+2], 16) {
            Ok(b) => out.push(b),
            Err(_) => return Vec::new(),
        }
    }
    out
}

fn remove_empty_dirs_recursive(dir: &std::path::Path) {
    if !dir.is_dir() { return; }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                remove_empty_dirs_recursive(&p);
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}


/// Bounds how many rechecks hash the disk at once. Recheck is triggered per
/// add / per explicit request (never a boot-wide O(N) scan), but a burst of
/// re-adds could otherwise thrash the disk -- cap concurrent checks.
fn recheck_sem() -> &'static Semaphore {
    static S: OnceLock<Semaphore> = OnceLock::new();
    S.get_or_init(|| Semaphore::new(2))
}

impl TorrentManager {
    /// True if the torrent's first file already exists on disk at its
    /// save_path. Cheap gate for auto-recheck: a fresh download with no data
    /// on disk returns false and skips the (all-miss) check.
    pub fn first_file_exists(&self, info_hash: &InfoHash) -> bool {
        let t = match self.get(info_hash) {
            Some(t) => t,
            None => return false,
        };
        let f0 = match t.meta.files.first() {
            Some(f) => f,
            None => return false,
        };
        let save_path = t.save_path.read().clone();
        let full = if t.meta.multi_file {
            save_path.join(&t.meta.name).join(&f0.path)
        } else {
            save_path.join(&f0.path)
        };
        full.exists()
    }

    /// Hash-check data already on disk and populate the picker with the pieces
    /// that verify. Runs in the background: the torrent shows status Checking
    /// until done, then Seeding (all pieces valid) or Downloading (the picker
    /// fetches whatever did not verify). Requires a picker (download-mode add);
    /// a seed_mode torrent has none and is rejected -- its data is trusted by
    /// explicit skip_checking, which stays the trust-fast path.
    pub fn recheck(self: &Arc<Self>, info_hash: &InfoHash) -> Result<(), String> {
        let t = self.get(info_hash).ok_or("torrent not found")?;
        // A seed_mode torrent has no picker (data trusted; no bitfield -> cheap at
        // scale). To recheck it, create an all-missing picker on demand so
        // run_recheck can populate it from disk. Only rechecked torrents pay for a
        // picker; download.rs gates piece requests on picker presence, so a recheck
        // that finds missing/corrupt pieces will refetch them.
        let _ = t.picker.get_or_init(|| {
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::torrent::piece_picker::PiecePicker::new(t.meta.num_pieces()),
            ))
        });
        if !t.is_paused.load(Ordering::Relaxed) {
            t.status
                .store(TorrentStatus::Checking as u8, Ordering::Relaxed);
        }
        let mgr = self.clone();
        let ih = *info_hash;
        tokio::spawn(async move {
            let _permit = recheck_sem().acquire().await;
            mgr.run_recheck(ih, t).await;
        });
        Ok(())
    }

    async fn run_recheck(&self, ih: InfoHash, t: Arc<TorrentState>) {
        let num_pieces = t.meta.num_pieces();
        let picker = match t.picker.get() {
            Some(p) => p.clone(),
            None => return,
        };
        // Recheck is authoritative: clear prior have bits, then re-derive from
        // disk. Otherwise a now-corrupt piece keeps its stale have bit.
        picker.lock().unwrap().reset_have();
        let mut have = 0u32;
        for piece in 0..num_pieces {
            if t.is_removed.load(Ordering::Relaxed) {
                return; // torrent removed mid-check -> drop out
            }
            if let Some(data) = crate::disk::read_piece_for_check(&t, piece).await {
                let expected = t.meta.pieces[piece as usize];
                let mut hasher = Sha1::new();
                hasher.update(&data);
                let mut computed = [0u8; 20];
                computed.copy_from_slice(&hasher.finalize());
                if computed == expected {
                    picker.lock().unwrap().set_have(piece);
                    have += 1;
                }
            }
        }

        let complete = num_pieces > 0 && have >= num_pieces;
        if t.is_paused.load(Ordering::Relaxed) {
            t.status
                .store(TorrentStatus::Stopped as u8, Ordering::Relaxed);
        } else if complete {
            if t.completed_time.load(Ordering::Relaxed) == 0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                t.completed_time.store(now, Ordering::Relaxed);
            }
            t.status
                .store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        } else {
            t.status
                .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        }

        // Persist the verified bitfield so a restart does not re-check from
        // scratch (mirrors the resume written at add / on piece completion).
        let bitfield = hex_encode_bytes(&picker.lock().unwrap().export_bitfield());
        let rd = fastresume::ResumeData {
            info_hash: hex_encode(&ih),
            torrent_path: t.torrent_file_path.clone(),
            save_path: t.save_path.read().to_string_lossy().to_string(),
            seed_mode: t.seed_mode,
            paused: t.is_paused.load(Ordering::Relaxed),
            total_uploaded: t.total_uploaded.load(Ordering::Relaxed),
            total_downloaded: t.total_downloaded.load(Ordering::Relaxed),
            added_time: t.added_time,
            completed_time: t.completed_time.load(Ordering::Relaxed),
            bitfield,
        };
        fastresume::save(&self.resume_dir, &ih, &rd);
        info!(
            "[recheck] {} verified {}/{} pieces -> {}",
            &hex_encode(&ih)[..8],
            have,
            num_pieces,
            if complete { "seeding" } else { "downloading" }
        );
    }
}
