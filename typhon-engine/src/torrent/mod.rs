pub mod meta;
pub mod metainfo;
pub mod piece_picker;
pub mod fastresume;
pub mod statedb;
pub mod rate;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use tracing::{info, warn};

use meta::{InfoHash, TorrentState, TorrentStatus};
use crate::disk::DiskManager;
use std::sync::OnceLock;
use futures::StreamExt;
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
    /// Durable per-torrent state. `None` only if SQLite could not be opened at
    /// all, in which case everything falls back to the legacy JSON directory
    /// so a broken database degrades into the old behaviour instead of losing
    /// state outright.
    state_db: Option<Arc<statedb::StateDb>>,
    /// Last state written per torrent, so a sweep can skip the rows that did
    /// not move. Without this the periodic save rewrites every torrent every
    /// five minutes regardless of activity, which is what made the old scheme
    /// expensive.
    last_saved: DashMap<InfoHash, statedb::Fingerprint>,
    /// Keep mirroring every record into the legacy `resume/` JSON directory.
    /// Off by default: the mirror costs one file write per torrent per sweep,
    /// which is the entire cost the state database exists to remove. Set
    /// TYPHON_RESUME_JSON=1 to keep both in step while validating.
    mirror_json: bool,
}

impl TorrentManager {
    pub fn new(data_dir: String, resume_dir: String, disk: Arc<DiskManager>) -> Self {
        std::fs::create_dir_all(&resume_dir).ok();
        let mirror_json = std::env::var("TYPHON_RESUME_JSON").map(|v| v == "1").unwrap_or(false);
        let state_db = if std::env::var("TYPHON_STATE_DB").map(|v| v == "0").unwrap_or(false) {
            info!("[statedb] disabled by TYPHON_STATE_DB=0, using legacy resume JSON only");
            None
        } else {
            // The database sits beside the resume directory, inside the
            // engine's own config folder: one engine, one file, which is what
            // makes relocating an engine a folder copy.
            let path = std::path::Path::new(&resume_dir)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("state.db");
            match statedb::StateDb::open(&path) {
                Ok(db) => {
                    info!("[statedb] opened {}", path.display());
                    Some(Arc::new(db))
                }
                Err(e) => {
                    warn!("[statedb] could not open {}: {} -- falling back to resume JSON", path.display(), e);
                    None
                }
            }
        };
        Self {
            torrents: DashMap::new(),
            data_dir,
            resume_dir,
            disk,
            upload_rate: rate::RateTracker::new(),
            download_rate: rate::RateTracker::new(),
            cached_unseeded_peers: std::sync::atomic::AtomicUsize::new(0),
            skey_index: DashMap::new(),
            state_db,
            last_saved: DashMap::new(),
            mirror_json,
        }
    }

    /// Persist one record now.
    ///
    /// Every path that used to call `fastresume::save` directly goes through
    /// here, so there is exactly one place that decides which backend is
    /// authoritative and whether the legacy mirror is being kept.
    fn persist(&self, ih: &InfoHash, rd: &fastresume::ResumeData) {
        match &self.state_db {
            Some(db) => {
                if let Err(e) = db.put(rd) {
                    // Do not lose the record over a transient database error:
                    // fall back to the JSON file for this one write, which the
                    // next start still knows how to read.
                    warn!("[statedb] put {} failed: {} -- writing JSON instead", &rd.info_hash[..8.min(rd.info_hash.len())], e);
                    fastresume::save(&self.resume_dir, ih, rd);
                    return;
                }
                // Record what is now on disk so the next sweep can tell this
                // torrent apart from one that has since moved. Absent during
                // add_torrent, where the state is not in the map yet -- the
                // first sweep then writes it once and starts tracking.
                if let Some(t) = self.get(ih) {
                    self.last_saved.insert(*ih, fingerprint_of(&t));
                }
                if self.mirror_json {
                    fastresume::save(&self.resume_dir, ih, rd);
                }
            }
            None => fastresume::save(&self.resume_dir, ih, rd),
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

    /// The disk manager every torrent here writes through. Needed by the
    /// subsystems that complete a piece without owning a peer connection.
    pub fn disk(&self) -> &Arc<DiskManager> {
        &self.disk
    }

    /// First torrent matching `pred`, walked in place. Deliberately not
    /// `all().into_iter().find()`: that clones an Arc for every torrent in
    /// the catalogue before looking at the first one.
    pub fn find_torrent<F>(&self, pred: F) -> Option<Arc<TorrentState>>
    where
        F: Fn(&Arc<TorrentState>) -> bool,
    {
        self.torrents
            .iter()
            .find(|r| pred(r.value()))
            .map(|r| r.value().clone())
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
            trackers: state.live_trackers.read().clone(),
        };
        self.persist(&ih, &rd);

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
        self.forget_state(info_hash);
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
                // is_removed is only observed when the get_peers stream next
                // yields, which may be never — cancel the task outright.
                crate::dht::untrack_torrent(info_hash);
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
        // Resuming re-arms the DHT stream that stop_torrent cancelled.
        crate::dht::track_torrent(t.clone());
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
        // A stopped torrent must not keep a get_peers recursion alive: the
        // stream loop only checks is_removed, which a stop does not set.
        crate::dht::untrack_torrent(info_hash);
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

    /// Read every durable record, from whichever backend holds them.
    ///
    /// An installation upgrading into the state database has a full `resume/`
    /// directory and an empty table; that case imports the directory once and
    /// carries on from the table. The JSON files are left on disk, so dropping
    /// back to an older build is just running it. Once the table has rows it
    /// is the only thing consulted -- a half-stale directory must never be
    /// allowed to resurrect torrents that were deleted since.
    fn load_state_records(&self) -> Vec<fastresume::ResumeData> {
        let db = match &self.state_db {
            Some(db) => db,
            None => return fastresume::load_all(&self.resume_dir),
        };
        if db.count() == 0 {
            let imported = db.import_legacy(&self.resume_dir);
            if imported > 0 {
                info!("[statedb] first start on SQLite state: {} records carried over", imported);
            }
        }
        db.load_all()
    }

    pub fn load_resume_data(&self) -> usize {
        // Timed in two parts on purpose. Reading the resume records is a few
        // dozen MB of small JSON; re-parsing every .torrent that each record
        // points at is orders of magnitude more bytes, because that is where
        // the piece hashes live. Without the split, a slow startup gets blamed
        // on whichever half is easier to imagine.
        let t_start = std::time::Instant::now();
        let resumes = self.load_state_records();
        let records = resumes.len();
        let t_records = t_start.elapsed();
        let mut loaded = 0;
        let mut parse_time = std::time::Duration::ZERO;
        let mut parse_bytes: u64 = 0;
        for rd in resumes {
            let t_parse = std::time::Instant::now();
            let parsed = metainfo::parse_torrent_file(&rd.torrent_path);
            parse_time += t_parse.elapsed();
            if let Ok(md) = std::fs::metadata(&rd.torrent_path) {
                parse_bytes += md.len();
            }
            let meta = match parsed {
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
            let saved_trackers = rd.trackers.clone();
            // Decode the bitfield here, before the meta is moved: a torrent that
            // is already complete gets no picker at all, instead of allocating
            // one, importing into it, being promoted to Seeding, and carrying it
            // for the life of the process.
            let resume_bits = hex_decode_bytes(&rd.bitfield);
            let already_complete = bitfield_is_complete(&resume_bits, meta.num_pieces());
            let state = TorrentState::new_with_times(
                meta,
                rd.save_path.into(),
                rd.torrent_path,
                rd.seed_mode,
                Some(rd.added_time),
                Some(rd.completed_time),
                !rd.seed_mode && !already_complete,
            );
            // The resume record wins over the .torrent: an edited list lives
            // here, and the file on disk may be the original one. Empty means
            // the torrent predates tracker editing, so the parsed list stands.
            if !saved_trackers.is_empty() {
                *state.live_trackers.write() = saved_trackers;
            }
            state.total_uploaded.store(rd.total_uploaded, Ordering::Relaxed);
            state.total_downloaded.store(rd.total_downloaded, Ordering::Relaxed);
            // Restore verified-pieces bitfield FIRST so we don't re-DL 6+ GB
            // on every restart AND so an already-complete torrent is
            // recognised as a seed below. Pre-bitfield resume files have an
            // empty string here (serde default) — old behaviour preserved.
            if !resume_bits.is_empty() {
                if let Some(picker) = state.picker.get() {
                    picker.lock().unwrap().import_bitfield(&resume_bits);
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
                state.release_have_tx();
            } else if state.picker.get().unwrap().lock().unwrap().is_complete() {
                state.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
                state.release_have_tx();
            } else {
                state.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
            }
            self.skey_index.insert(crate::crypto::mse::sha1_combine(b"req2", &ih), ih);
            let state = Arc::new(state);
            // Seed the sweep's baseline from what was just read back, so the
            // first sweep after a start writes only what has moved since --
            // otherwise every start would rewrite the whole table once.
            self.last_saved.insert(ih, fingerprint_of(&state));
            self.torrents.insert(ih, state);
            loaded += 1;
        }
        info!(
            "[resume] startup: {} records read in {:.2}s, {} .torrent re-parsed in {:.2}s ({:.1} MiB), {} loaded, total {:.2}s",
            records,
            t_records.as_secs_f64(),
            records,
            parse_time.as_secs_f64(),
            parse_bytes as f64 / 1048576.0,
            loaded,
            t_start.elapsed().as_secs_f64(),
        );
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

    /// Persist current state for every torrent that changed since the last
    /// sweep, in one transaction.
    ///
    /// The old version wrote every torrent every time. At 200k torrents on a
    /// five-minute tick that is ~666 file rewrites per second forever, almost
    /// all of them byte-identical to what was already on disk, each one
    /// dirtying a filesystem block that the next snapshot then pins. Comparing
    /// a six-field fingerprint first reduces the sweep to the torrents that
    /// actually moved -- the hot set, not the total.
    pub fn save_all_resume(&self) {
        let db = match &self.state_db {
            Some(db) => db.clone(),
            None => {
                // Legacy path, unchanged.
                for entry in self.torrents.iter() {
                    let t = entry.value();
                    let rd = Self::build_resume_data(t);
                    fastresume::save(&self.resume_dir, &t.info_hash, &rd);
                }
                return;
            }
        };

        let started = std::time::Instant::now();
        let mut dirty: Vec<fastresume::ResumeData> = Vec::new();
        let mut hashes: Vec<InfoHash> = Vec::new();
        let mut fps: Vec<statedb::Fingerprint> = Vec::new();
        let total = self.torrents.len();
        for entry in self.torrents.iter() {
            let t = entry.value();
            let fp = fingerprint_of(t);
            if self.last_saved.get(&t.info_hash).map(|p| *p.value() == fp).unwrap_or(false) {
                continue;
            }
            dirty.push(Self::build_resume_data(t));
            hashes.push(t.info_hash);
            fps.push(fp);
        }
        if dirty.is_empty() {
            return;
        }
        let mirror = self.mirror_json;
        match db.put_batch(&dirty) {
            Ok(n) => {
                for (ih, fp) in hashes.iter().zip(fps.iter()) {
                    self.last_saved.insert(*ih, *fp);
                }
                if mirror {
                    for (rd, ih) in dirty.iter().zip(hashes.iter()) {
                        fastresume::save(&self.resume_dir, ih, rd);
                    }
                }
                info!(
                    "[statedb] sweep: {} of {} torrents changed, committed in {:.3}s",
                    n,
                    total,
                    started.elapsed().as_secs_f64()
                );
            }
            Err(e) => {
                // Never drop a sweep silently: fall back to the JSON files for
                // this round rather than leave the changes unpersisted.
                warn!("[statedb] sweep commit failed: {} -- writing {} JSON records instead", e, dirty.len());
                for (rd, ih) in dirty.iter().zip(hashes.iter()) {
                    fastresume::save(&self.resume_dir, ih, rd);
                }
            }
        }
    }

    /// Build a fastresume snapshot from the live TorrentState. Shared
    /// between save_all_resume (periodic) and set_save_path (immediate
    /// flush after a category move).
    /// Write one torrent's record to the store right now.
    pub fn persist_completed(&self, ih: &InfoHash) {
        if let Some(t) = self.get(ih) {
            let rd = Self::build_resume_data(&t);
            self.persist(ih, &rd);
        }
    }

    fn build_resume_data(t: &TorrentState) -> fastresume::ResumeData {
        let bitfield = match t.picker.get() {
            Some(p) => hex_encode_bytes(&p.lock().unwrap().export_bitfield()),
            // A seed_mode torrent is trusted complete on load without reading
            // any bitfield, so an empty one is the intended encoding.
            None if t.seed_mode => String::new(),
            // No picker and not seed_mode means the torrent was loaded complete
            // -- that is the invariant behind skipping the allocation
            // (alloc_picker = !seed_mode && !already_complete).
            //
            // Writing an empty bitfield here ERASED that fact:
            // bitfield_is_complete("") is false, so the next boot rebuilt the
            // torrent at 0%, allocated an all-missing picker and re-downloaded
            // data already sitting on disk. Since last_saved is empty at boot,
            // the first sweep marked every torrent dirty and did this to the
            // whole catalogue. Emit the full bitfield the picker would have.
            None => hex_encode_bytes(&full_bitfield(t.meta.num_pieces())),
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
            trackers: t.live_trackers.read().clone(),
        }
    }

    /// Swap the in-memory save_path for a running torrent and flush
    /// fastresume so the change survives a crash before the next
    /// periodic save. Caller (hydra-go) must have stopped the torrent
    /// and moved the files on disk *before* invoking this.
    /// Replace the tracker list and flush the resume record immediately, so
    /// the change survives a crash before the next periodic save. Mirrors
    /// set_save_path: mutate, then persist, in one call the caller cannot
    /// forget half of.
    pub fn set_trackers(&self, info_hash: &InfoHash, tiers: Vec<Vec<String>>) -> Result<(), String> {
        let t = self.get(info_hash).ok_or("torrent not found")?;
        *t.live_trackers.write() = tiers;
        let rd = Self::build_resume_data(&t);
        self.persist(info_hash, &rd);
        Ok(())
    }

    pub fn set_save_path(&self, info_hash: &InfoHash, new_path: &str) -> Result<(), String> {
        let t = self.get(info_hash).ok_or("torrent not found")?;
        *t.save_path.write() = new_path.into();
        let rd = Self::build_resume_data(&t);
        self.persist(info_hash, &rd);
        Ok(())
    }

    /// Drop every trace of a torrent's durable state.
    ///
    /// Both backends are cleared unconditionally, including the legacy JSON
    /// file even when the mirror is off: production accumulated thousands of
    /// orphaned resume files precisely because a removal path forgot one of
    /// the places state lived. Deleting from a place that has nothing is free.
    fn forget_state(&self, info_hash: &InfoHash) {
        if let Some(db) = &self.state_db {
            if let Err(e) = db.remove(&hex_encode(info_hash)) {
                warn!("[statedb] remove {} failed: {}", &hex_encode(info_hash)[..8], e);
            }
        }
        self.last_saved.remove(info_hash);
        fastresume::remove(&self.resume_dir, info_hash);
    }

    /// Export one torrent's durable state, for handing it to another engine.
    /// Returns None if the torrent is not held here.
    pub fn export_state(&self, info_hash: &InfoHash) -> Option<fastresume::ResumeData> {
        self.get(info_hash).map(|t| Self::build_resume_data(&t))
    }

    /// Adopt a torrent from another engine, progression and all.
    ///
    /// This is the receiving half of a move. It is deliberately the same code
    /// path a restart takes -- the record that crosses between engines is the
    /// exact record that would have been written to disk and read back -- so a
    /// torrent that moves engines is indistinguishable from one that restarted.
    /// The alternative, re-adding and re-checking, would re-hash every byte on
    /// disk for a torrent that never lost a piece.
    ///
    /// The caller is responsible for having already moved (or decided not to
    /// move) the payload files, and for removing the torrent from the source
    /// engine afterwards: this end only adopts.
    pub fn import_state(&self, rd: &fastresume::ResumeData) -> Result<(InfoHash, String), String> {
        let meta = metainfo::parse_torrent_file(&rd.torrent_path)
            .map_err(|e| format!("import: parse {}: {}", rd.torrent_path, e))?;
        let ih = meta.info_hash;
        let name = meta.name.clone();
        if self.torrents.contains_key(&ih) {
            return Err("torrent already present in this engine".into());
        }

        let resume_bits = hex_decode_bytes(&rd.bitfield);
        let already_complete = bitfield_is_complete(&resume_bits, meta.num_pieces());
        let state = TorrentState::new_with_times(
            meta,
            rd.save_path.clone().into(),
            rd.torrent_path.clone(),
            rd.seed_mode,
            Some(rd.added_time),
            Some(rd.completed_time),
            !rd.seed_mode && !already_complete,
        );
        // An edited tracker list lives in the record, not in the .torrent on
        // disk. Dropping it here would silently undo the edit on every move.
        if !rd.trackers.is_empty() {
            *state.live_trackers.write() = rd.trackers.clone();
        }
        state.total_uploaded.store(rd.total_uploaded, Ordering::Relaxed);
        state.total_downloaded.store(rd.total_downloaded, Ordering::Relaxed);
        // Bitfield before status, for the same reason the startup path does it
        // in that order: the status is derived from completeness.
        if !resume_bits.is_empty() {
            if let Some(picker) = state.picker.get() {
                picker.lock().unwrap().import_bitfield(&resume_bits);
            }
        }
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

        let state = Arc::new(state);
        let total_size = state.meta.total_size;
        let num_pieces = state.meta.num_pieces();
        let private = state.meta.private;
        self.skey_index.insert(crate::crypto::mse::sha1_combine(b"req2", &ih), ih);
        self.torrents.insert(ih, state.clone());
        crate::dht::track_torrent(state);
        // Durable here before the source is told to let go, so a crash in the
        // middle leaves the torrent in both engines rather than in neither.
        self.persist(&ih, rd);
        crate::rpc::events::publish(crate::rpc::events::Event::TorrentAdded {
            info_hash: hex_encode(&ih),
            name: name.clone(),
            save_path: rd.save_path.clone(),
            total_size,
            num_pieces,
            private,
            seed_mode: rd.seed_mode,
        });
        Ok((ih, name))
    }
}

/// Summarise the parts of a torrent's live state that are worth persisting.
///
/// Read straight off atomics plus one short lock on the picker, so this is
/// cheap enough to run for every torrent on every sweep -- which is the point:
/// it is what lets the sweep skip the ones that have not moved. The verified
/// piece count stands in for the bitfield itself, because the bitfield cannot
/// change without the count changing too, and comparing a u32 is free next to
/// exporting and comparing a multi-kilobyte bitfield per torrent.
fn fingerprint_of(t: &TorrentState) -> statedb::Fingerprint {
    statedb::Fingerprint {
        total_uploaded: t.total_uploaded.load(Ordering::Relaxed),
        total_downloaded: t.total_downloaded.load(Ordering::Relaxed),
        completed_time: t.completed_time.load(Ordering::Relaxed),
        num_have: t.picker.get().map(|p| p.lock().unwrap().num_have()).unwrap_or(0),
        paused: t.is_paused.load(Ordering::Relaxed),
        seed_mode: t.seed_mode,
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

/// Is every piece already present in this resume bitfield?
///
/// Answered without building a `PiecePicker`, which is the whole point: the
/// picker is what we are trying not to allocate. A short or empty bitfield is
/// "not complete" -- the same conservative reading `import_bitfield` gives it.
/// Torrents that just finished downloading, waiting to be written to the store.
///
/// A completion used to update memory only and rely on the five-minute sweep.
/// Anything that stopped the engine inside that window -- a deploy, a crash,
/// an OOM -- lost the fact that the torrent was complete, and the next boot
/// re-downloaded every byte of it. The manager drains this and persists at
/// once.
static COMPLETED: OnceLock<tokio::sync::mpsc::UnboundedSender<InfoHash>> = OnceLock::new();
static COMPLETED_RX: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<InfoHash>>> =
    std::sync::Mutex::new(None);

fn completed_channel() -> &'static tokio::sync::mpsc::UnboundedSender<InfoHash> {
    COMPLETED.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *COMPLETED_RX.lock().unwrap() = Some(rx);
        tx
    })
}

/// Signal that a torrent finished downloading. Cheap and non-blocking.
pub fn notify_completed(ih: InfoHash) {
    let _ = completed_channel().send(ih);
}

/// Taken once, by the task that persists completions.
pub fn take_completion_receiver() -> Option<tokio::sync::mpsc::UnboundedReceiver<InfoHash>> {
    completed_channel();
    COMPLETED_RX.lock().unwrap().take()
}

/// Every piece present, in the bit order `bitfield_is_complete` reads.
fn full_bitfield(num_pieces: u32) -> Vec<u8> {
    let n = num_pieces as usize;
    if n == 0 {
        return Vec::new();
    }
    let mut bytes = vec![0xFFu8; n.div_ceil(8)];
    let rem = n % 8;
    if rem != 0 {
        if let Some(last) = bytes.last_mut() {
            *last = !((1u8 << (8 - rem)) - 1);
        }
    }
    bytes
}

fn bitfield_is_complete(bytes: &[u8], num_pieces: u32) -> bool {
    let n = num_pieces as usize;
    if n == 0 || bytes.is_empty() {
        return false;
    }
    for i in 0..n {
        let byte_idx = i / 8;
        let bit_idx = 7 - (i % 8);
        if byte_idx >= bytes.len() || (bytes[byte_idx] >> bit_idx) & 1 == 0 {
            return false;
        }
    }
    true
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
    // Deux rechecks simultanes lisant leurs pieces une par une plafonnaient a
    // ~2 Mo/s sur un pool qui en sert 500. Reparer 573 torrents aurait pris des
    // milliers d heures. Reglable sans rebuild pour une campagne de reparation.
    S.get_or_init(|| {
        let n = std::env::var("TYPHON_RECHECK_TORRENTS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(4);
        Semaphore::new(n)
    })
}

impl TorrentManager {
    /// True if the torrent's first file already exists on disk at its
    /// save_path. Cheap gate for auto-recheck: a fresh download with no data
    /// on disk returns false and skips the (all-miss) check.
    /// True when AT LEAST ONE of the torrent's files is present on disk.
    ///
    /// This used to probe files[0] alone, which was wrong in the exact case it
    /// mattered: a partial download very often lacks precisely the first file
    /// (priority set to zero, or a non-sequential order that never reached
    /// it). The probe then answered "nothing on disk" for a torrent holding
    /// most of its data, the recheck was skipped, and every piece was
    /// re-downloaded over bytes that were already there. Measured on a
    /// three-file torrent with two files present: 67% when the missing one was
    /// last, 0% when it was first.
    ///
    /// Short-circuits on the first hit, so the common case costs one stat.
    pub fn any_file_exists(&self, info_hash: &InfoHash) -> bool {
        let t = match self.get(info_hash) {
            Some(t) => t,
            None => return false,
        };
        let save_path = t.save_path.read().clone();
        t.meta.files.iter().any(|f| {
            let full = if t.meta.multi_file {
                save_path.join(&t.meta.name).join(&f.path)
            } else {
                save_path.join(&f.path)
            };
            full.exists()
        })
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
        // Recheck is authoritative, but it must not publish a verdict it does
        // not have yet. Clearing the live picker up front left the torrent at
        // 0% for the entire scan, and the five-minute state sweep -- whose
        // dirty check is num_have -- persisted that empty bitfield within
        // minutes. A 160 GiB torrent takes hours to verify, so any restart in
        // that window resurrected a complete torrent as 0% and re-downloaded
        // data already sitting on disk. Verify into a local set and install it
        // in one critical section once the verdict is final.
        // Une piece a la fois laissait le disque a l arret entre deux lectures :
        // mesure a 0,5 piece/s (~2 Mo/s) la ou le pool en sert plusieurs
        // centaines. Les lectures partent en parallele, le hachage suit, et
        // c est le disque qui redevient le facteur limitant.
        let width = std::env::var("TYPHON_RECHECK_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(16);
        let mut verified: Vec<u32> = futures::stream::iter(0..num_pieces)
            .map(|piece| {
                let t = t.clone();
                async move {
                    if t.is_removed.load(Ordering::Relaxed) {
                        return None;
                    }
                    let data = crate::disk::read_piece_for_check(&t, piece).await?;
                    // No hash table -> we cannot say this piece is good. Leave
                    // the have bit clear; the recheck reports the torrent
                    // incomplete rather than blessing unverified data.
                    let expected = t.piece_hash(piece)?;
                    let mut hasher = Sha1::new();
                    hasher.update(&data);
                    let mut computed = [0u8; 20];
                    computed.copy_from_slice(&hasher.finalize());
                    if computed == expected { Some(piece) } else { None }
                }
            })
            .buffer_unordered(width)
            .filter_map(|r| async move { r })
            .collect()
            .await;
        if t.is_removed.load(Ordering::Relaxed) {
            return; // torrent removed mid-check -> do not publish a verdict
        }
        verified.sort_unstable();
        let have = verified.len() as u32;
        {
            let mut p = picker.lock().unwrap();
            p.reset_have();
            for piece in &verified {
                p.set_have(*piece);
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
            // The recheck was the only reader; a seeder needs no hashes.
            t.release_piece_hashes();
            t.release_have_tx();
        } else {
            // Still incomplete: the download path is about to verify every
            // piece it pulls, so the table stays loaded.
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
            trackers: t.live_trackers.read().clone(),
        };
        self.persist(&ih, &rd);
        info!(
            "[recheck] {} verified {}/{} pieces -> {}",
            &hex_encode(&ih)[..8],
            have,
            num_pieces,
            if complete { "seeding" } else { "downloading" }
        );
    }
}

#[cfg(test)]
mod picker_alloc_tests {
    use super::bitfield_is_complete;

    /// A full bitfield must be recognised without a picker: that recognition is
    /// exactly what lets the loader skip the allocation.
    #[test]
    fn full_bitfield_is_complete() {
        assert!(bitfield_is_complete(&[0b1111_1111], 8));
        assert!(bitfield_is_complete(&[0xFF, 0b1100_0000], 10));
    }

    /// One missing piece anywhere must keep the picker alive, otherwise a
    /// partial download would be silently promoted to "seeding" and never
    /// finish.
    #[test]
    fn one_hole_is_not_complete() {
        assert!(!bitfield_is_complete(&[0b1111_1110], 8));
        assert!(!bitfield_is_complete(&[0b0111_1111], 8));
        assert!(!bitfield_is_complete(&[0xFF, 0b1000_0000], 10));
    }

    /// Short, empty or zero-piece bitfields are "not complete" -- the same
    /// conservative reading import_bitfield gives them.
    #[test]
    fn short_or_empty_is_not_complete() {
        assert!(!bitfield_is_complete(&[], 8));
        assert!(!bitfield_is_complete(&[0xFF], 16));
        assert!(!bitfield_is_complete(&[0xFF], 0));
    }
}

#[cfg(test)]
mod full_bitfield_tests {
    use super::{bitfield_is_complete, full_bitfield};

    /// The bitfield persisted for a complete, picker-less torrent must read
    /// back as complete. When it did not, every boot re-downloaded the whole
    /// catalogue.
    #[test]
    fn full_bitfield_reads_back_complete() {
        for n in [1u32, 7, 8, 9, 10, 63, 64, 65, 38067] {
            let bf = full_bitfield(n);
            assert!(bitfield_is_complete(&bf, n), "n={}", n);
        }
    }

    /// Guard the tail mask: bits past num_pieces must stay clear, otherwise a
    /// short torrent would claim pieces it does not have.
    #[test]
    fn tail_bits_beyond_num_pieces_are_clear() {
        assert_eq!(full_bitfield(10), vec![0xFF, 0b1100_0000]);
        assert_eq!(full_bitfield(8), vec![0xFF]);
        assert!(full_bitfield(0).is_empty());
    }
}
