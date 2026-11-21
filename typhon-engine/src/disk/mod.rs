use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
#[cfg(test)]
use std::time::Duration;

pub static FD_CACHE_HIT: AtomicU64 = AtomicU64::new(0);
pub static FD_CACHE_MISS: AtomicU64 = AtomicU64::new(0);

/// Global cached File handles, keyed by absolute path.
/// Avoids open()/close() syscall storm under high concurrent peer load.
fn fd_cache() -> &'static Mutex<LruCache<PathBuf, Arc<File>>> {
    static CACHE: OnceLock<Mutex<LruCache<PathBuf, Arc<File>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LruCache::new(NonZeroUsize::new(10_000).unwrap())))
}

/// Get or open a File for this path, cached as Arc<File>.
fn get_or_open(path: &PathBuf) -> std::io::Result<Arc<File>> {
    {
        let mut cache = fd_cache().lock();
        if let Some(f) = cache.get(path) {
            FD_CACHE_HIT.fetch_add(1, Ordering::Relaxed);
            return Ok(f.clone());
        }
    }
    FD_CACHE_MISS.fetch_add(1, Ordering::Relaxed);
    // Open read+write (not read-only) even on the read path. The fd_cache is
    // GLOBAL and keyed by path only, shared with get_or_open_rw. If a reader
    // (tit-for-tat serve of an already-have piece) cached an O_RDONLY handle
    // first, a later write to that same not-yet-complete file reused the RO fd
    // and pwrite failed with EBADF, wedging the piece into an infinite
    // re-download loop.
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(f) => Arc::new(f),
        // A file we may read but not write -- a read-only mount, a 0444 seed
        // -- is still served, from a read-only handle that is NOT cached: a
        // later write to it opens its own, and fails for the real reason.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            return std::fs::File::open(path).map(Arc::new).map_err(|_| e);
        }
        Err(e) => return Err(e),
    };
    let mut cache = fd_cache().lock();
    cache.put(path.clone(), f.clone());
    Ok(f)
}

/// Get or open a File for read+write, creating if missing. Eliminates
/// the per-piece OpenOptions::open() syscall storm in write_piece (was
/// one open() per 1MB piece — for a 4 GB transfer that's ~4096 syscalls
/// on the leecher side, dominating tokio scheduling overhead).
fn get_or_open_rw(path: &PathBuf) -> std::io::Result<Arc<File>> {
    {
        let mut cache = fd_cache().lock();
        if let Some(f) = cache.get(path) {
            FD_CACHE_HIT.fetch_add(1, Ordering::Relaxed);
            return Ok(f.clone());
        }
    }
    FD_CACHE_MISS.fetch_add(1, Ordering::Relaxed);
    let f = Arc::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Never truncate: the file may already hold pieces.
            .truncate(false)
            .open(path)?,
    );
    let mut cache = fd_cache().lock();
    cache.put(path.clone(), f.clone());
    Ok(f)
}

/// `get_or_open_rw`, but always a handle on the file the path names now, which
/// then replaces whatever the cache held for it.
fn open_rw_fresh(path: &PathBuf) -> std::io::Result<Arc<File>> {
    let f = Arc::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?,
    );
    fd_cache().lock().put(path.clone(), f.clone());
    Ok(f)
}

// ---------------------------------------------------------------------------
// `[daemon] preallocate`: reserve a torrent's files at its first write.
// ---------------------------------------------------------------------------

static PREALLOCATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PREALLOC_UNSUPPORTED_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set once at startup from `[daemon] preallocate`. Global, like the fd cache
/// it sits beside: the setting is the daemon's, not a torrent's.
pub fn set_preallocate(on: bool) {
    PREALLOCATE.store(on, Ordering::Relaxed);
}

/// What one torrent still has to reserve: every wanted file (priority above 0)
/// with a length, as a path below the save path and its declared size, and the
/// fingerprint of that set. Empty when the option is off or the set is the one
/// already reserved.
fn prealloc_plan(
    torrent: &TorrentState,
    save_path: &std::path::Path,
) -> (Vec<(PathBuf, u64)>, u64) {
    if !PREALLOCATE.load(Ordering::Relaxed) {
        return (Vec::new(), 0);
    }
    // FNV-1a over the save path and the indices of the wanted files; the
    // result is kept off 0, which means "never reserved".
    let print = torrent.with_file_priorities(|prio| {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in save_path.as_os_str().as_encoded_bytes() {
            h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        for (i, f) in torrent.meta.files.iter().enumerate() {
            let p = prio
                .get(i)
                .copied()
                .unwrap_or(crate::torrent::piece_picker::PRIO_NORMAL);
            if f.length > 0 && p != 0 {
                h = (h ^ (i as u64 + 1)).wrapping_mul(0x100000001b3);
            }
        }
        h | 1
    });
    if torrent.prealloc_print.load(Ordering::Relaxed) == print {
        return (Vec::new(), print);
    }
    let order = torrent.download_order();
    let plan = torrent
        .meta
        .files
        .iter()
        .enumerate()
        .filter(|(i, f)| f.length > 0 && order.file_priority(*i) != 0)
        .map(|(i, f)| (torrent.meta.disk_rel(i), f.length))
        .collect();
    (plan, print)
}

/// Reserve `len` bytes of `file` with `fallocate(2)` mode 0: it extends, never
/// truncates and never rewrites a byte that is there. NOT `posix_fallocate`,
/// which glibc answers with a write of zeros when the file system cannot.
#[cfg(target_os = "linux")]
fn reserve(file: &File, len: u64) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    if file.metadata()?.len() >= len {
        return Ok(());
    }
    loop {
        // SAFETY: a valid open descriptor, and plain integers.
        let r = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len as libc::off_t) };
        if r == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

/// Reserve `len` bytes of `file` by growing it to `len` (`set_len`, which is
/// `SetEndOfFile`): NTFS allocates the clusters, and the allocation lasts. A
/// bare allocation size (`FileAllocationInfo`) does not: NTFS hands back what
/// lies past the end of file when the last handle closes, and the descriptor
/// cache closes handles on its own. Never shorter than the file is already,
/// no `SetFileValidData` (it needs an administrator privilege): a write past
/// the old end of file is zero-filled by Windows, with or without this call.
#[cfg(windows)]
fn reserve(file: &File, len: u64) -> std::io::Result<()> {
    if file.metadata()?.len() >= len {
        return Ok(());
    }
    file.set_len(len)
}

/// The bytes the file system has set aside for `file` (its allocation size).
/// Only the tests ask.
#[cfg(all(windows, test))]
fn win_allocated(file: &File) -> std::io::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_STANDARD_INFO,
    };
    // SAFETY: zeroed plain integers, filled by the call below.
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: a valid open handle and a buffer of the size announced.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileStandardInfo,
            &mut info as *mut FILE_STANDARD_INFO as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.AllocationSize.max(0) as u64)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn reserve(_file: &File, _len: u64) -> std::io::Result<()> {
    Ok(())
}

/// Whether `[daemon] preallocate` does anything on this system: Linux
/// (`fallocate`) and Windows (the file grown to its size). Elsewhere the switch is a
/// no-op, and the Settings tab says so.
pub const PREALLOCATE_SUPPORTED: bool = cfg!(any(target_os = "linux", windows));

/// Reserve every file of `plan`. `skip` names the ones that must not be
/// touched before they are compared (another torrent or hardlink names them).
/// ENOSPC fails with a message that says so; a file system with no fallocate
/// is said once and the write goes on.
fn preallocate(
    save_path: &std::path::Path,
    plan: &[(PathBuf, u64)],
    skip: impl Fn(&std::path::Path) -> bool,
) -> Result<(), String> {
    for (rel, len) in plan {
        if skip(rel) {
            continue;
        }
        let full = save_path.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create folder {:?}: {}", parent, e))?;
        }
        let file = get_or_open_rw(&full).map_err(|e| format!("open write {:?}: {}", full, e))?;
        match reserve(&file, *len) {
            Ok(()) => {}
            Err(e) => {
                #[cfg(unix)]
                let code = e.raw_os_error();
                #[cfg(not(unix))]
                let code: Option<i32> = e.raw_os_error();
                #[cfg(target_os = "linux")]
                {
                    if matches!(code, Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS)) {
                        if !PREALLOC_UNSUPPORTED_WARNED.swap(true, Ordering::Relaxed) {
                            tracing::warn!(
                                path = %full.display(),
                                "preallocate: this file system does not support reserving space \
                                 (fallocate); files are written as they come, nothing is reserved"
                            );
                        }
                        // The same file system answers the same for the rest.
                        return Ok(());
                    }
                    if code == Some(libc::ENOSPC) {
                        return Err(format!(
                            "not enough space to reserve {:?} ({} bytes): {}",
                            full, len, e
                        ));
                    }
                }
                #[cfg(windows)]
                {
                    use windows_sys::Win32::Foundation::{
                        ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL, ERROR_INVALID_FUNCTION,
                        ERROR_NOT_SUPPORTED,
                    };
                    if matches!(
                        code,
                        Some(c) if c == ERROR_INVALID_FUNCTION as i32 || c == ERROR_NOT_SUPPORTED as i32
                    ) {
                        if !PREALLOC_UNSUPPORTED_WARNED.swap(true, Ordering::Relaxed) {
                            tracing::warn!(
                                path = %full.display(),
                                "preallocate: this file system does not support reserving space \
                                 (growing the file); files are written as they come, nothing is reserved"
                            );
                        }
                        return Ok(());
                    }
                    if matches!(
                        code,
                        Some(c) if c == ERROR_DISK_FULL as i32 || c == ERROR_HANDLE_DISK_FULL as i32
                    ) {
                        return Err(format!(
                            "not enough space to reserve {:?} ({} bytes): {}",
                            full, len, e
                        ));
                    }
                }
                let _ = code;
                return Err(format!("reserve {:?} ({} bytes): {}", full, len, e));
            }
        }
    }
    Ok(())
}

use bytes::Bytes;
use lru::LruCache;
use std::num::NonZeroUsize;

use crate::torrent::meta::TorrentState;

pub mod readcache;
pub use readcache::ReadCache;
use readcache::SLICE;

/// Evict cached File handles for these paths so the underlying `Arc<File>` is
/// dropped and the kernel reclaims the blocks of a just-unlinked file.
/// `remove_torrent` unlinks files but, without this, the cached fd pins the
/// inode (and its disk blocks) alive until LRU eviction — a space leak that
/// fills /race under drain churn (10 000 entries: a drain unlinks far fewer
/// before the LRU gets round to them).
pub fn evict_fds(paths: &[PathBuf]) {
    let _ = evict_fds_timed(paths);
}

/// `evict_fds`, returning (time waiting for the fd cache lock, time holding
/// it). The cache is process-wide and every piece read goes through it, so a
/// slow removal can be queued here behind the serve path.
pub fn evict_fds_timed(paths: &[PathBuf]) -> (std::time::Duration, std::time::Duration) {
    let asked = std::time::Instant::now();
    let mut cache = fd_cache().lock();
    let got = std::time::Instant::now();
    for p in paths {
        cache.pop(p);
    }
    drop(cache);
    (got - asked, got.elapsed())
}

/// `File::sync_data`, with a seam for tests: a disk that refuses to flush
/// cannot be had from a unit test, so a path can be told to fail here.
#[cfg(not(test))]
fn sync_data(file: &File, _path: &std::path::Path) -> std::io::Result<()> {
    file.sync_data()
}

#[cfg(test)]
fn sync_data(file: &File, path: &std::path::Path) -> std::io::Result<()> {
    if sync_failures().lock().contains(path) {
        return Err(std::io::Error::from_raw_os_error(5));
    }
    file.sync_data()
}

#[cfg(test)]
fn sync_failures() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static F: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Make every later sync of `path` fail with EIO.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn fail_sync_of(path: PathBuf) {
    sync_failures().lock().insert(path);
}

// A one-off shape; naming it would add an alias that nothing else uses.
#[allow(clippy::type_complexity)]
pub struct DiskManager {
    /// What `read_block` answers from: the process's own in production,
    /// shared by every engine (`with_cache`), a private one otherwise.
    cache: Arc<ReadCache>,
    /// Files written and not yet synced, per torrent, with the pieces that
    /// went to each: a file that then fails to sync says which pieces it
    /// takes down with it.
    written: Arc<Mutex<HashMap<[u8; 20], HashMap<PathBuf, Vec<u32>>>>>,
    /// Asked, once per torrent, which of its files other torrents name (paths
    /// below its save path). Set by the host that knows the other torrents;
    /// without it only a hardlink protects a file.
    claim_probe: OnceLock<ClaimProbe>,
    /// Read-aheads under way for this engine (`prefetch_next`), at most
    /// `readcache::PREFETCH_MAX`.
    prefetches: Arc<AtomicUsize>,
}

/// The files other torrents name, for the torrent with this info hash.
pub type ClaimProbe = Arc<dyn Fn(&[u8; 20]) -> std::collections::HashSet<PathBuf> + Send + Sync>;

/// What a sync pass did: how many files it made durable, and the files that
/// refused, with the pieces written to them since the last good sync.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub synced: usize,
    pub failed: Vec<SyncFailure>,
}

/// A file that would not sync. Its `pieces` are in the page cache at best:
/// nothing vouches for them on disk, and trying again vouches for nothing
/// either (after a failed flush the kernel may drop the dirty pages and
/// report the next one clean).
#[derive(Debug)]
pub struct SyncFailure {
    pub info_hash: [u8; 20],
    pub path: PathBuf,
    pub error: String,
    pub pieces: Vec<u32>,
}

impl DiskManager {
    /// `max_open_files` is not used: descriptors live in the process-wide
    /// cache (`fd_cache`), fixed at 10 000. The per-manager pool this sized
    /// was never read, and a setting that looks like it acts is worse than
    /// none.
    pub fn new(max_open_files: usize) -> Self {
        Self::with_cache(
            max_open_files,
            Arc::new(ReadCache::new(
                readcache::DEFAULT_MB << 20,
                readcache::DEFAULT_SHARDS,
            )),
        )
    }

    /// Serving through `cache`, which other managers may share.
    pub fn with_cache(_max_open_files: usize, cache: Arc<ReadCache>) -> Self {
        Self {
            cache,
            written: Arc::new(Mutex::new(HashMap::new())),
            claim_probe: OnceLock::new(),
            prefetches: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The cache `read_block` answers from.
    pub fn read_cache(&self) -> &Arc<ReadCache> {
        &self.cache
    }

    /// Say how to find the files another torrent names. Once; later calls are
    /// ignored.
    pub fn set_claim_probe(&self, probe: ClaimProbe) {
        let _ = self.claim_probe.set(probe);
    }

    /// Make every file written since the last sync durable, and say how many
    /// did, and which did not.
    ///
    /// Called by the resume sweep before it saves the bitfields. SQLite syncs
    /// its own file, not ours: without this, after a power cut the record
    /// could be on disk while the bytes it vouches for were still in the page
    /// cache, and we would serve zeros as verified pieces. Each file is synced
    /// once however many pieces went to it, and the set holds only what was
    /// written since the last sweep; most of it has reached the disk on its own
    /// by then. Blocking: callers run it off the async workers.
    pub fn sync_written(&self) -> SyncReport {
        let all = std::mem::take(&mut *self.written.lock());
        let mut report = SyncReport::default();
        for (ih, files) in all {
            let one = self.sync_files(ih, files);
            report.synced += one.synced;
            report.failed.extend(one.failed);
        }
        report
    }

    /// The torrents that wrote a piece since their last sync: the quick resume
    /// pass reads this instead of walking the catalogue. Takes nothing out.
    pub fn written_torrents(&self) -> Vec<[u8; 20]> {
        self.written.lock().keys().copied().collect()
    }

    /// `sync_written` for one torrent: what its completion record needs.
    pub fn sync_torrent(&self, info_hash: &[u8; 20]) -> SyncReport {
        let files = self.written.lock().remove(info_hash);
        files.map_or_else(SyncReport::default, |f| self.sync_files(*info_hash, f))
    }

    /// A file that fails to sync is NOT put back to be tried again: a second
    /// attempt can succeed with the data gone. It is reported with its pieces
    /// instead, and the caller stops counting them as held before any state
    /// vouching for them is saved.
    fn sync_files(&self, info_hash: [u8; 20], files: HashMap<PathBuf, Vec<u32>>) -> SyncReport {
        let mut report = SyncReport::default();
        for (path, mut pieces) in files {
            let cached = fd_cache().lock().peek(&path).cloned();
            // Never created here: a file deleted since was meant to go.
            let file = match cached {
                Some(f) => f,
                None => match std::fs::OpenOptions::new().write(true).open(&path) {
                    Ok(f) => Arc::new(f),
                    Err(_) => continue,
                },
            };
            match sync_data(&file, &path) {
                Ok(()) => report.synced += 1,
                Err(e) => {
                    tracing::warn!("[disk] sync {:?}: {}", path, e);
                    pieces.sort_unstable();
                    pieces.dedup();
                    report.failed.push(SyncFailure {
                        info_hash,
                        path,
                        error: e.to_string(),
                        pieces,
                    });
                }
            }
        }
        report
    }

    /// Read a block for a peer's request, through the read cache: the 128 KiB
    /// slice around it is read once and kept a minute (`readcache`), so the
    /// next seven 16 KiB requests of that slice come out of memory. Requests
    /// for a slice that is being read wait on that read rather than start
    /// their own. The answer shares the slice's memory: no copy.
    pub async fn read_block(
        &self,
        torrent: &TorrentState,
        piece: u32,
        offset: u32,
        length: u32,
    ) -> Result<Bytes, String> {
        let piece_size = torrent.meta.piece_length as u64;
        let abs_start = piece as u64 * piece_size + offset as u64;
        let slice_start = (abs_start / SLICE) * SLICE;
        let at = (abs_start - slice_start) as usize;
        let end = at + length as usize;

        // A request across two slices, or larger than one: read alone.
        if (length as u64) > SLICE || end > SLICE as usize {
            readcache::BYPASS.fetch_add(1, Ordering::Relaxed);
            return read_block_direct(torrent, piece, offset, length).await;
        }

        let key = readcache::Key {
            info_hash: torrent.info_hash,
            start: slice_start,
        };
        let cut = |slice: &Bytes| {
            if end > slice.len() {
                return Err(format!(
                    "short slice: need {}..{} got {}",
                    at,
                    end,
                    slice.len()
                ));
            }
            Ok(slice.slice(at..end))
        };
        if let Some(slice) = self.cache.get(&key) {
            return cut(&slice);
        }

        let guard = self.cache.flight(key);
        let mut ran = false;
        let read = guard
            .flight
            .get_or_init(|| {
                ran = true;
                readcache::MISS.fetch_add(1, Ordering::Relaxed);
                read_slice(&self.cache, torrent, key, false)
            })
            .await
            .clone();
        drop(guard);
        if !ran {
            readcache::JOINED.fetch_add(1, Ordering::Relaxed);
            self.cache.used(&key);
        }
        match read {
            Ok(slice) => cut(&slice),
            // The slice reaches into sectors this request may not need: ask
            // for the request alone before blaming anything, and blame only
            // what it spans. Every request waiting on the slice does this for
            // its own part -- there is no slice to share them out of.
            Err(f) if f.unreadable => match read_range(torrent, piece, offset, length).await {
                Ok(b) => Ok(b),
                Err(e) => Err(give_up_on(torrent, piece, offset, length, e)),
            },
            Err(f) => Err(give_up_on(torrent, piece, offset, length, f)),
        }
    }

    /// Read the next slice of this piece into the cache, in the background,
    /// for a peer whose request here followed its last one (`Walk::step`):
    /// its next requests then find the slice in memory, or join the read
    /// already under way, rather than each new slice waiting a seek. Called
    /// after `read_block` answered the request, and only then: a block served
    /// by sendfile was in the page cache, where the kernel's own readahead
    /// already works.
    ///
    /// The read is the buffered one `read_block` does, into the read cache,
    /// so on ZFS the bytes still come through the ARC, which is why that
    /// path is taken there (see `path_is_zfs`).
    ///
    /// Skipped when the next slice is past this piece (the peer may want
    /// another piece next), held, already being read, asked for this peer
    /// already, or when the engine has `PREFETCH_MAX` read-aheads under way.
    /// A read-ahead that fails is dropped: the request that wants the slice
    /// reads it, and only then is anything blamed. The handle is for tests.
    pub fn prefetch_next(
        &self,
        walk: &mut readcache::Walk,
        torrent: &Arc<TorrentState>,
        piece: u32,
        offset: u32,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let plen = torrent.meta.piece_length as u64;
        let piece_start = piece as u64 * plen;
        let piece_end = piece_start + torrent.meta.piece_size(piece) as u64;
        let start = ((piece_start + offset as u64) / SLICE + 1) * SLICE;
        if start >= piece_end || walk.asked == Some(start) {
            return None;
        }
        let slot = PrefetchSlot::take(&self.prefetches)?;
        walk.asked = Some(start);
        let key = readcache::Key {
            info_hash: torrent.info_hash,
            start,
        };
        let flight = self.cache.begin_prefetch(key)?;
        let cache = self.cache.clone();
        let torrent = torrent.clone();
        Some(tokio::spawn(async move {
            let _slot = slot;
            let guard = cache.adopt(key, flight);
            let _ = guard
                .flight
                .get_or_init(|| {
                    readcache::PREFETCH_ISSUED.fetch_add(1, Ordering::Relaxed);
                    read_slice(&cache, &torrent, key, true)
                })
                .await;
        }))
    }

    /// Write a complete piece to disk and verify SHA1.
    pub async fn write_piece(
        &self,
        torrent: &TorrentState,
        piece: u32,
        data: Vec<u8>,
    ) -> Result<bool, String> {
        // A seed-only torrent never writes its data: no file, no directory, no
        // preallocation, before anything below can touch the disk.
        if torrent.is_seed_only() {
            return Err(format!(
                "{} is seed-only; refusing to write piece {}",
                torrent.info_hash_hex(),
                piece
            ));
        }
        // Invalidate cached read blocks overlapping this piece
        let piece_size = torrent.meta.piece_length as u64;
        let piece_start = piece as u64 * piece_size;
        let piece_end = piece_start + data.len() as u64;
        let first_block = (piece_start / SLICE) * SLICE;
        self.cache
            .invalidate(torrent.info_hash, first_block, piece_end);

        let ops = torrent.meta.map_block(piece, 0, data.len() as u32);
        let save_path = torrent.save_path.read().clone();
        let (prealloc, prealloc_print) = prealloc_plan(torrent, &save_path);
        let prealloc_planned = !prealloc.is_empty();
        let written = self.written.clone();
        let info_hash = torrent.info_hash;

        // A piece we cannot check is a piece we refuse, not one we accept.
        let check = match torrent.piece_check(piece) {
            Some(c) => c,
            None => {
                return Err(format!(
                    "no piece hashes for {}; refusing to accept piece {}",
                    torrent.info_hash_hex(),
                    piece
                ))
            }
        };

        let guard = torrent.guarded();
        let verdicts = torrent.write_verdicts();
        let probe = self.claim_probe.get().cloned();
        // The free links this piece touches (see `FreeLinks`), with the
        // pieces of each: nearly always none, and then nothing below changes.
        let free = torrent.free_links();
        let plen = torrent.meta.piece_length as u64;
        let free_spans: Vec<(PathBuf, u64, u64)> = match &free {
            None => Vec::new(),
            Some(free) => {
                let f = free.lock();
                if f.linked.is_empty() && f.undone.is_empty() {
                    Vec::new()
                } else {
                    ops.iter()
                        .filter(|op| {
                            !op.pad && (f.linked.contains(&op.path) || f.undone.contains(&op.path))
                        })
                        .filter_map(|op| {
                            file_span(&torrent.meta, &op.path).map(|(o, l)| (op.path.clone(), o, l))
                        })
                        .collect()
                }
            }
        };
        let picker = torrent.picker.get().cloned();
        let (free_task, spans_task) = (free.clone(), free_spans.clone());
        let outcome = tokio::task::spawn_blocking(move || {
            if !check.matches(&data) {
                return Ok(false);
            }
            // A piece over a free link is written by one task at a time, from
            // the comparison to the last byte: two edge pieces of the same
            // file must not both decide on the linked file, nor one compare
            // with it while the other replaces it.
            let mut free_now = free_task
                .as_ref()
                .filter(|_| !spans_task.is_empty())
                .map(|f| f.lock());
            let mut give_up: Vec<PathBuf> = Vec::new();
            // Before a byte goes down: the ranges that fall in a file this
            // torrent must not write into (another torrent names it, or it has
            // another hardlink) are read back. Identical bytes need no write
            // and get none; one that differs refuses the whole piece, so a
            // piece never lands half in a guarded file and half beside it.
            let mut same = vec![false; ops.len()];
            {
                let mut at = 0usize;
                for (i, op) in ops.iter().enumerate() {
                    let len = op.length as usize;
                    // A free link given up already: the file under that name
                    // is the torrent's own download, guarded only as any file
                    // it created (another name appearing, another torrent).
                    let undone = free_now
                        .as_ref()
                        .is_some_and(|f| f.undone.contains(&op.path));
                    // Whatever the process remembers: a torrent resumed after
                    // a restart has no guard, and a file that was already
                    // shared with a library when the torrent started writing
                    // is shared all the same.
                    if !op.pad
                        && ((!undone && guard.contains(&op.path))
                            || must_compare(&verdicts, &probe, &info_hash, &save_path, &op.path))
                    {
                        let full_path = save_path.join(&op.path);
                        match holds_bytes(&full_path, op.file_offset, &data[at..at + len]) {
                            Ok(Some(true)) => same[i] = true,
                            // Not there: creating it overwrites nothing.
                            Ok(None) => {}
                            // A free file the proof linked in: the piece is
                            // right (hashed above), so the file is not this
                            // torrent's after all. Given up below, once the
                            // rest of the piece is known to be writable.
                            Ok(Some(false))
                                if !undone
                                    && free_now.as_ref().is_some_and(|f| {
                                        f.at == save_path && f.linked.contains(&op.path)
                                    }) =>
                            {
                                give_up.push(op.path.clone());
                            }
                            Ok(Some(false)) => {
                                return Err(format!(
                                    "{REFUSED} {}: piece {piece} carries other bytes than the file \
                                     holds, and another torrent or another hardlink names that \
                                     file. It was left untouched",
                                    op.path.display()
                                ))
                            }
                            Err(e) => {
                                let name = op.path.display();
                                return Err(format!("{REFUSED} {name}: cannot read it: {e}"));
                            }
                        }
                    }
                    at += len;
                }
            }
            if let Some(f) = free_now.as_mut() {
                for rel in &give_up {
                    let pieces = spans_task
                        .iter()
                        .find(|(p, _, _)| p == rel)
                        .filter(|(_, _, l)| *l > 0 && plen > 0)
                        .map(|(_, o, l)| {
                            ((o / plen) as u32..=((o + l - 1) / plen) as u32).collect()
                        })
                        .unwrap_or_default();
                    give_up_free_link(
                        f,
                        &verdicts,
                        picker.as_deref(),
                        &save_path,
                        rel,
                        pieces,
                        piece,
                    )?;
                }
            }
            // The first piece of a torrent in this process reserves ALL its
            // wanted files, so a disk too small fails here, not half way. The
            // torrent is marked (in its state, once the piece is down) only
            // after every file is done: after an ENOSPC the next piece tries again. A file wanted later is reserved at
            // the next write (the set's fingerprint moved).
            if !prealloc.is_empty() {
                preallocate(&save_path, &prealloc, |rel| {
                    guard.contains(rel)
                        || must_compare(&verdicts, &probe, &info_hash, &save_path, rel)
                })?;
            }
            let mut data_offset = 0usize;
            for (i, op) in ops.into_iter().enumerate() {
                // Alignment padding: zeros no file holds.
                if op.pad {
                    data_offset += op.length as usize;
                    continue;
                }
                // Already there, byte for byte: the piece counts, the file is
                // not opened for writing.
                if same[i] {
                    data_offset += op.length as usize;
                    continue;
                }
                // `op.path` is below the save path, wherever the file was put.
                let full_path = save_path.join(&op.path);

                // Said as what it is: swallowed, a folder that could not be
                // made (permissions, quota, a file in the way) came back as
                // an open error that no longer named the cause.
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("create folder {:?}: {}", parent, e))?;
                }

                // A free link given up: never through a cached handle, which
                // a read may have opened on the linked file before its name
                // went, and which would write into the original.
                let given_up = free_now
                    .as_ref()
                    .is_some_and(|f| f.undone.contains(&op.path));
                let file = if given_up {
                    open_rw_fresh(&full_path)
                } else {
                    get_or_open_rw(&full_path)
                }
                .map_err(|e| format!("open write {:?}: {}", full_path, e))?;

                {
                    let chunk = &data[data_offset..data_offset + op.length as usize];
                    pwrite_all(&file, chunk, op.file_offset)
                        .map_err(|e| format!("pwrite {:?}: {}", full_path, e))?;
                }
                {
                    let mut w = written.lock();
                    let pieces = w
                        .entry(info_hash)
                        .or_default()
                        .entry(full_path)
                        .or_default();
                    if pieces.last() != Some(&piece) {
                        pieces.push(piece);
                    }
                }

                data_offset += op.length as usize;
            }

            Ok(true)
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?;
        // A piece that went down passed the reservation: remember it, in the
        // state, so only a new state (or a new set of wanted files) redoes it.
        if prealloc_planned && matches!(outcome, Ok(true)) {
            torrent
                .prealloc_print
                .store(prealloc_print, Ordering::Relaxed);
        }
        // A free link given up (or found given up by another piece): its
        // name is no longer under the guard the proof set, and nothing cached
        // of the linked file's bytes may be served for the new one.
        if let Some(free) = free.as_ref().filter(|_| !free_spans.is_empty()) {
            // Once per file, not at every piece: lifting the guard and
            // dropping the whole file from the read cache costs in proportion
            // to the file, and doing it at every piece grew with the square of
            // its size. The file stays in `undone`, so every write into it
            // still opens it fresh (see above): only this cleanup is settled.
            let undone: Vec<(PathBuf, u64, u64)> = {
                let f = free.lock();
                free_spans
                    .iter()
                    .filter(|(p, _, _)| f.undone.contains(p) && !f.settled.contains(p))
                    .cloned()
                    .collect()
            };
            for (rel, offset, len) in &undone {
                torrent.unguard_file(rel);
                let from = (offset / SLICE) * SLICE;
                self.cache.invalidate(torrent.info_hash, from, offset + len);
            }
            // Settled only once a piece went down into the new file; a write
            // that failed leaves it to the next piece.
            if matches!(outcome, Ok(true)) && !undone.is_empty() {
                let mut f = free.lock();
                for (rel, _, _) in &undone {
                    if f.settled.insert(rel.clone()) {
                        f.settlements += 1;
                    }
                }
            }
        }
        if let Err(why) = &outcome {
            if why.starts_with(REFUSED) {
                // Not a disk fault to retry three times: the file is not ours
                // to write, and no other peer's copy of the piece changes that.
                torrent.mark_write_error(why);
            }
        }
        // Again once the bytes are down: a read that went to disk while the
        // write was under way may have read the old bytes, and must not leave
        // them cached over the piece just written.
        self.cache.bump(torrent.info_hash, first_block, piece_end);
        self.cache
            .invalidate(torrent.info_hash, first_block, piece_end);
        outcome
    }

    /// Resolve a piece block to a single backing file handle + byte offset for
    /// the sendfile serve path. None when the block spans more than one file
    /// (caller falls back to the buffered read+send path).
    pub fn block_file(
        &self,
        torrent: &TorrentState,
        piece: u32,
        offset: u32,
        length: u32,
    ) -> Option<(Arc<File>, u64)> {
        let ops = torrent.meta.map_block(piece, offset, length);
        if ops.len() != 1 {
            return None;
        }
        let op = &ops[0];
        if op.length != length || op.pad {
            return None;
        }
        let save_path = torrent.save_path.read().clone();
        let full_path = save_path.join(&op.path);
        get_or_open(&full_path).ok().map(|f| (f, op.file_offset))
    }
}

/// Whether a piece written into `rel` must be compared with the disk, found out
/// once per file and kept in `verdicts`. A file that is not there is the
/// torrent's own to create: no. One that is there is protected when another
/// name points at it (a link count above one) or another torrent names it.
/// A normal download costs one `stat` per file, the first time it is written.
///
/// A link that appears WHILE the torrent downloads (an import hardlinking the
/// incomplete file) is not seen: the verdict was taken at the first piece. That
/// is acceptable, because what the torrent writes into its own file is bytes
/// that matched the piece hash. The hole is for a file that was ALREADY shared
/// when the torrent started writing, and that is what this decides.
fn must_compare(
    verdicts: &parking_lot::Mutex<crate::torrent::meta::WriteVerdicts>,
    probe: &Option<ClaimProbe>,
    info_hash: &[u8; 20],
    save_path: &std::path::Path,
    rel: &std::path::Path,
) -> bool {
    if let Some(known) = verdicts.lock().files.get(rel).copied() {
        return known;
    }
    // (verdict, whether it may be kept). Nothing here holds the lock: the probe
    // can take a while and every other piece of the torrent asks the same cache.
    let (verdict, keep) = match std::fs::metadata(save_path.join(rel)) {
        Err(_) => (false, true),
        Ok(m) if linked_elsewhere(&m) => (true, true),
        Ok(_) => match probe {
            // No way to ask yet (the host installs it after the engines
            // start): "nobody names it" would be a guess, so it is neither
            // believed nor kept -- the next piece asks again.
            None => (false, false),
            Some(probe) => {
                let known = verdicts.lock().claimed.clone();
                let claimed = match known {
                    Some(c) => c,
                    None => {
                        let c = Arc::new(probe(info_hash));
                        verdicts.lock().claimed.get_or_insert(c).clone()
                    }
                };
                (claimed.contains(rel), true)
            }
        },
    };
    if keep {
        verdicts.lock().files.insert(rel.to_path_buf(), verdict);
    }
    verdict
}

#[cfg(unix)]
fn linked_elsewhere(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    m.nlink() > 1
}

#[cfg(not(unix))]
fn linked_elsewhere(_m: &std::fs::Metadata) -> bool {
    false
}

/// How a refusal to write into a guarded file starts.
const REFUSED: &str = "refused to overwrite";

/// Where the file `rel` (below the save path) lies in the torrent: (offset,
/// length).
fn file_span(
    meta: &crate::torrent::meta::TorrentMeta,
    rel: &std::path::Path,
) -> Option<(u64, u64)> {
    meta.files
        .iter()
        .enumerate()
        .find(|(i, _)| meta.disk_rel(*i) == rel)
        .map(|(_, f)| (f.offset, f.length))
}

/// Give up the free link `rel` (see `FreeLinks`): a verified piece carries
/// other bytes than it holds, so it is not the torrent's file.
///
/// In this order: every other piece that touches the file stops counting as
/// held (the serve path asks the picker before it reads, so none of them is
/// offered from the file about to go); the cached handle is closed; the
/// torrent's own name for it, below its save path, is removed -- the original
/// name, elsewhere, keeps the inode and its bytes, back to one link; the
/// verdict kept for the file is forgotten, so the file the piece now creates
/// under that name is judged as any file the torrent creates. The piece being
/// written is not cleared: it is about to land in the new file. A name that
/// cannot be removed puts the pieces back and refuses the piece as before.
fn give_up_free_link(
    free: &mut crate::torrent::meta::FreeLinks,
    verdicts: &parking_lot::Mutex<crate::torrent::meta::WriteVerdicts>,
    picker: Option<&std::sync::Mutex<crate::torrent::piece_picker::PiecePicker>>,
    save_path: &std::path::Path,
    rel: &std::path::Path,
    pieces: Vec<u32>,
    piece: u32,
) -> Result<(), String> {
    let pieces: Vec<u32> = pieces.into_iter().filter(|p| *p != piece).collect();
    let mut cleared: Vec<u32> = Vec::new();
    if let Some(picker) = picker {
        let mut p = picker.lock().unwrap_or_else(|e| e.into_inner());
        for x in &pieces {
            if p.has_piece(*x) {
                p.clear_have(*x);
                cleared.push(*x);
            }
        }
    }
    let full_path = save_path.join(rel);
    evict_fds(std::slice::from_ref(&full_path));
    match std::fs::remove_file(&full_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            if let Some(picker) = picker {
                let mut p = picker.lock().unwrap_or_else(|e| e.into_inner());
                for x in &cleared {
                    p.set_have(*x);
                }
            }
            return Err(format!(
                "{REFUSED} {}: it holds other bytes than the torrent and its link could not be \
                 removed: {e}",
                rel.display()
            ));
        }
    }
    verdicts.lock().files.remove(rel);
    free.linked.remove(rel);
    free.undone.insert(rel.to_path_buf());
    tracing::warn!(
        file = %full_path.display(),
        piece,
        pieces_missing_again = cleared.len(),
        "path claim: a file reused from the disk holds other bytes than piece {piece}; its link \
         is removed (the original is left as it was) and it is downloaded on its own"
    );
    Ok(())
}

/// Whether the file at `path` holds exactly `want` from `offset`: `None` when
/// there is no such file, `Some(false)` when it is shorter than that or any
/// byte differs. Reads through a handle of its own, read-only: never the cached
/// one, which may be on a file replaced since.
fn holds_bytes(path: &std::path::Path, offset: u64, want: &[u8]) -> std::io::Result<Option<bool>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut buf = vec![0u8; want.len().min(1 << 20)];
    let mut done = 0usize;
    while done < want.len() {
        let n = (want.len() - done).min(buf.len());
        match pread_exact(&file, &mut buf[..n], offset + done as u64) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Some(false)),
            Err(e) => return Err(e),
        }
        if buf[..n] != want[done..done + n] {
            return Ok(Some(false));
        }
        done += n;
    }
    Ok(Some(true))
}

/// Stream a piece block straight from the page cache to a plaintext TCP peer
/// via `sendfile(2)`, skipping the userspace copies (read cache -> Bytes ->
/// codec buffer -> kernel) the buffered path pays. Writes the 13-byte piece
/// header first, then splices the body.
///
/// `Ok(true)` = whole block served; `Ok(false)` = declined without touching
/// the wire (non-unix); `Err` = failed after the header went out, so the
/// stream is mid-message and the caller MUST drop the peer.
/// True if `path` lives on a ZFS filesystem (statfs magic). Serving from ZFS
/// goes through the buffered read()+cache path so blocks flow through the ARC:
/// sendfile/splice bypass the ARC on ZoL, so a zero-copy serve would dump the
/// served hot-set into dumb page-cache LRU and starve the ARC. Non-ZFS backends
/// (e.g. NVMe/XFS) keep the zero-copy fast path (CPU-bound, ARC irrelevant).
#[cfg(unix)]
pub fn path_is_zfs(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let c = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    st.f_type as u64 == 0x2fc12fc1 // ZFS_SUPER_MAGIC
}
#[cfg(not(unix))]
pub fn path_is_zfs(_path: &std::path::Path) -> bool {
    false
}

/// Whether a block is in the page cache, asked without blocking: preadv2 with
/// RWF_NOWAIT of one byte, on the block's first page and on its last. True ->
/// safe to sendfile from the reactor. Cold -> the caller takes the buffered
/// path, whose read runs on the blocking pool.
///
/// The first page alone was asked before. A block whose tail was not cached
/// passed, and sendfile then waited on the disk inside an async worker,
/// stalling every peer on that worker. The middle pages are not asked: a
/// 16 KiB block is four pages, readahead brings them in order, and a block
/// with both ends cached and a hole between them is not worth a syscall per
/// page on every request.
#[cfg(unix)]
pub fn is_block_resident(file: &File, offset: u64, len: u64) -> bool {
    let last = offset.saturating_add(len.max(1) - 1);
    page_resident(file, offset) && (last / PAGE == offset / PAGE || page_resident(file, last))
}

/// The smallest page size Linux runs with. On a kernel with larger pages two
/// offsets this calls apart may share a page: the probe is then asked twice,
/// never skipped.
#[cfg(unix)]
const PAGE: u64 = 4096;

#[cfg(unix)]
fn page_resident(file: &File, offset: u64) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut b = [0u8; 1];
    let iov = libc::iovec {
        iov_base: b.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let Ok(at) = libc::off_t::try_from(offset) else {
        return false;
    };
    // RWF_NOWAIT (0x8): returns EAGAIN instead of blocking if not in page cache.
    // SAFETY: a valid open descriptor, and one iovec over `b`, a live 1-byte
    // buffer the call may write into.
    let n = unsafe { libc::preadv2(file.as_raw_fd(), &iov, 1, at, 0x8) };
    n == 1
}

#[cfg(not(unix))]
pub fn is_block_resident(_file: &File, _offset: u64, _len: u64) -> bool {
    false
}

#[cfg(unix)]
pub async fn serve_block_sendfile(
    sock: &tokio::net::TcpStream,
    file: &File,
    offset: u64,
    index: u32,
    begin: u32,
    len: usize,
) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    use tokio::io::Interest;

    let mut header = [0u8; 13];
    header[0..4].copy_from_slice(&((9 + len) as u32).to_be_bytes());
    header[4] = 7; // MSG_PIECE
    header[5..9].copy_from_slice(&index.to_be_bytes());
    header[9..13].copy_from_slice(&begin.to_be_bytes());

    let mut hoff = 0usize;
    while hoff < header.len() {
        sock.writable().await?;
        match sock.try_io(Interest::WRITABLE, || {
            let n = unsafe {
                libc::send(
                    sock.as_raw_fd(),
                    header[hoff..].as_ptr() as *const libc::c_void,
                    header.len() - hoff,
                    libc::MSG_NOSIGNAL,
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "send header 0",
                ))
            }
            Ok(n) => hoff += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }

    let file_fd = file.as_raw_fd();
    let mut off = offset as libc::off_t;
    let mut remaining = len;
    while remaining > 0 {
        sock.writable().await?;
        match sock.try_io(Interest::WRITABLE, || {
            let n = unsafe { libc::sendfile(sock.as_raw_fd(), file_fd, &mut off, remaining) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "sendfile 0",
                ))
            }
            Ok(n) => remaining -= n,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(not(unix))]
pub async fn serve_block_sendfile(
    _sock: &tokio::net::TcpStream,
    _file: &File,
    _offset: u64,
    _index: u32,
    _begin: u32,
    _len: usize,
) -> std::io::Result<bool> {
    Ok(false)
}

/// One of an engine's `PREFETCH_MAX` read-ahead places, given back when
/// the read-ahead ends, however it ends.
struct PrefetchSlot(Arc<AtomicUsize>);

impl PrefetchSlot {
    fn take(count: &Arc<AtomicUsize>) -> Option<Self> {
        // A bound, not a hand-off: nothing is published through it. Taken,
        // then given back when it went over.
        let slot = PrefetchSlot(count.clone());
        (count.fetch_add(1, Ordering::Relaxed) < readcache::PREFETCH_MAX).then_some(slot)
    }
}

impl Drop for PrefetchSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One slice from disk, kept unless a write went by while it was read.
/// `prefetched`: read ahead, not asked for (see `prefetch_next`).
async fn read_slice(
    cache: &ReadCache,
    torrent: &TorrentState,
    key: readcache::Key,
    prefetched: bool,
) -> Result<Bytes, ReadFailure> {
    let seen = cache.generation(&key);
    let piece_size = torrent.meta.piece_length as u64;
    let slice_end = (key.start + SLICE).min(torrent.meta.total_size);
    // A slice past the end must not wrap the subtraction into a huge read.
    let Some(len) = slice_end.checked_sub(key.start) else {
        return Err(ReadFailure {
            missing: false,
            unreadable: false,
            msg: format!("slice start {} past end {}", key.start, slice_end),
        });
    };
    // Lossless: `start` is inside the torrent, so its piece is one of the
    // torrent's (a u32 on the wire), its offset is below a piece length
    // (a u32 in the metainfo) and `len` is at most SLICE.
    let piece = (key.start / piece_size) as u32;
    let in_piece = (key.start % piece_size) as u32;
    let slice = read_range(torrent, piece, in_piece, len as u32).await?;
    if !cache.keep(key, slice.clone(), seen, prefetched) && prefetched {
        readcache::PREFETCH_WASTED.fetch_add(1, Ordering::Relaxed);
    }
    Ok(slice)
}

async fn read_block_direct(
    torrent: &TorrentState,
    piece: u32,
    offset: u32,
    length: u32,
) -> Result<Bytes, String> {
    read_range(torrent, piece, offset, length)
        .await
        .map_err(|e| give_up_on(torrent, piece, offset, length, e))
}

/// Why a read for a peer failed.
#[derive(Clone, Debug)]
struct ReadFailure {
    /// A backing file is gone (ENOENT): the data is gone for good.
    missing: bool,
    /// The file is there and the read itself failed (EIO, a short file): what
    /// it holds there cannot be trusted or served.
    unreadable: bool,
    msg: String,
}

/// What a failed read does to the torrent. ENOENT parks it once (the serve
/// path then rejects upstream and stops getting here at all). An unreadable
/// range takes the pieces it spans out of what we hold -- they are not offered
/// or served again, and a recheck brings them back -- and the torrent's error
/// says so. EMFILE and the like are transient and must NOT drag a healthy
/// torrent down with them.
fn give_up_on(
    torrent: &TorrentState,
    piece: u32,
    offset: u32,
    length: u32,
    e: ReadFailure,
) -> String {
    if e.missing {
        torrent.mark_error(&e.msg);
    } else if e.unreadable {
        let plen = torrent.meta.piece_length as u64;
        let start = piece as u64 * plen + offset as u64;
        let last = (start + (length as u64).max(1) - 1) / plen;
        let pieces: Vec<u32> = (start / plen..=last).map(|p| p as u32).collect();
        torrent.retract_pieces(
            &pieces,
            &format!(
                "unreadable on disk, piece {} no longer counted as held: {}. Check the disk, \
                 then recheck this torrent",
                pieces
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                e.msg
            ),
        );
    }
    e.msg
}

async fn read_range(
    torrent: &TorrentState,
    piece: u32,
    offset: u32,
    length: u32,
) -> Result<Bytes, ReadFailure> {
    let ops = torrent.meta.map_block(piece, offset, length);
    let save_path = torrent.save_path.read().clone();
    // `op.path` is below the save path: save_path/<name>/<file-path> for a
    // multi-file torrent (`files` key, even with one entry), save_path/<name>
    // for a single-file one, or wherever the file was renamed to.
    tokio::task::spawn_blocking(move || {
        let mut result = Vec::with_capacity(length as usize);

        for op in ops {
            if op.pad {
                result.resize(result.len() + op.length as usize, 0);
                continue;
            }
            let full_path = save_path.join(&op.path);

            // Keep the ErrorKind. ENOENT means the data is gone for good and
            // the torrent has to be parked; EMFILE or EIO are transient and
            // must NOT drag a healthy torrent down with them.
            let file = get_or_open(&full_path).map_err(|e| ReadFailure {
                missing: e.kind() == std::io::ErrorKind::NotFound,
                unreadable: false,
                msg: format!("open {:?}: {}", full_path, e),
            })?;

            let mut buf = vec![0u8; op.length as usize];

            serve_read(&file, &full_path, &mut buf, op.file_offset).map_err(|e| ReadFailure {
                missing: false,
                unreadable: true,
                msg: match classify_io(e) {
                    PieceRead::Unreadable(why) => format!("pread {:?}: {}", full_path, why),
                    _ => format!("pread {:?}: the file ends early", full_path),
                },
            })?;

            result.extend_from_slice(&buf);
        }

        Ok::<Bytes, ReadFailure>(Bytes::from(result))
    })
    .await
    .map_err(|e| ReadFailure {
        missing: false,
        unreadable: false,
        msg: format!("spawn_blocking: {}", e),
    })?
}

/// `pread_exact` for the serve path, with a seam for tests: unreadable sectors
/// cannot be had from a unit test, so a path can be told to fail here.
#[cfg(not(test))]
fn serve_read(
    file: &File,
    _path: &std::path::Path,
    buf: &mut [u8],
    offset: u64,
) -> std::io::Result<()> {
    pread_exact(file, buf, offset)
}

#[cfg(test)]
fn serve_read(
    file: &File,
    path: &std::path::Path,
    buf: &mut [u8],
    offset: u64,
) -> std::io::Result<()> {
    *served_reads().lock().entry(path.to_path_buf()).or_default() += 1;
    if read_failures().lock().contains(path) {
        return Err(std::io::Error::from_raw_os_error(5));
    }
    pread_exact(file, buf, offset)
}

/// How many served reads went to each path's disk.
#[cfg(test)]
fn served_reads() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn read_failures() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static F: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Make every later served read of `path` fail with EIO.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn fail_read_of(path: PathBuf) {
    read_failures().lock().insert(path);
}

// Cross-platform positional file I/O. Unix uses pread/pwrite (FileExt);
// Windows uses seek_read/seek_write (looped to match all/exact semantics).
#[cfg(unix)]
fn pwrite_all(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}
#[cfg(windows)]
fn pwrite_all(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0usize;
    while written < buf.len() {
        let n = file.seek_write(&buf[written..], offset + written as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "seek_write returned 0",
            ));
        }
        written += n;
    }
    Ok(())
}
#[cfg(unix)]
fn pread_exact(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}
#[cfg(windows)]
fn pread_exact(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        let n = file.seek_read(&mut buf[read..], offset + read as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "seek_read returned 0",
            ));
        }
        read += n;
    }
    Ok(())
}

/// What reading a piece back for a hash check came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PieceRead {
    /// Read whole; whether it matches its hash.
    Checked(bool),
    /// Not there to be read: a backing file is missing or too short, or there
    /// is no hash to check against. The piece is simply not had.
    Absent,
    /// The disk refused the read (an I/O error, a file that cannot be opened).
    /// Says nothing about the piece itself.
    Unreadable(String),
}

/// A file that is not there, or ends early, is a piece not had; any other
/// failure is the disk's. One rule for the recheck and for the serve path.
fn classify_io(e: std::io::Error) -> PieceRead {
    match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::UnexpectedEof => PieceRead::Absent,
        _ => PieceRead::Unreadable(e.to_string()),
    }
}

/// Read a full piece from disk and check it against its hash, both on the
/// blocking pool: hashing a piece of several MiB on an async worker held that
/// worker for the whole digest. Bypasses the read cache: a one-shot cold
/// recheck scan should not evict the hot blocks we are actively serving.
pub async fn read_piece_on_disk(torrent: &TorrentState, piece: u32) -> PieceRead {
    let plen = torrent.meta.piece_size(piece);
    if plen == 0 {
        return PieceRead::Absent;
    }
    let Some(check) = torrent.piece_check(piece) else {
        return PieceRead::Absent;
    };
    let ops = torrent.meta.map_block(piece, 0, plen);
    let save_path = torrent.save_path.read().clone();

    let out = tokio::task::spawn_blocking(move || {
        let classify = classify_io;
        // Read straight into the piece: one buffer, no copy per file.
        let mut data = vec![0u8; plen as usize];
        let mut at = 0usize;
        for op in ops {
            let len = op.length as usize;
            if !op.pad {
                let full_path = save_path.join(&op.path);
                let file = match get_or_open(&full_path) {
                    Ok(f) => f,
                    Err(e) => return classify(e),
                };
                let Some(buf) = data.get_mut(at..at + len) else {
                    return PieceRead::Absent;
                };
                if let Err(e) = pread_exact(&file, buf, op.file_offset) {
                    return classify(e);
                }
            }
            at += len;
        }
        PieceRead::Checked(check.matches(&data))
    })
    .await;
    out.unwrap_or(PieceRead::Absent)
}

/// `read_piece_on_disk` as the callers that only ask "do we have it" see it:
/// `None` when the piece cannot be said to be had, for whatever reason.
pub async fn check_piece_on_disk(torrent: &TorrentState, piece: u32) -> Option<bool> {
    match read_piece_on_disk(torrent, piece).await {
        PieceRead::Checked(ok) => Some(ok),
        PieceRead::Absent | PieceRead::Unreadable(_) => None,
    }
}

#[cfg(test)]
mod disk_tests {
    use super::*;
    use crate::torrent::TorrentManager;
    use std::sync::Arc;

    fn sha1_of(data: &[u8]) -> [u8; 20] {
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(data);
        h.finalize().into()
    }

    /// A single-file torrent whose piece hash is the REAL hash of `content`.
    /// A fixture with a made-up hash can only ever exercise the refusal path.
    // No caller today. Kept, not deleted: clearing lints is not the moment to drop code.
    #[allow(dead_code)]
    fn torrent_bytes(name: &str, content: &[u8]) -> Vec<u8> {
        // One piece, sized to the content, so the whole file is piece 0.
        torrent_bytes_in_pieces(name, content, content.len())
    }

    fn torrent_bytes_in_pieces(name: &str, content: &[u8], piece_len: usize) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(
            format!("d6:lengthi{}e4:name{}:{name}", content.len(), name.len()).as_bytes(),
        );
        let hashes: Vec<u8> = content.chunks(piece_len).flat_map(sha1_of).collect();
        info.extend_from_slice(
            format!("12:piece lengthi{piece_len}e6:pieces{}:", hashes.len()).as_bytes(),
        );
        info.extend_from_slice(&hashes);
        info.push(b'e');
        let mut out = Vec::new();
        out.extend_from_slice(b"d4:info");
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    struct Fixture {
        _mgr: Arc<TorrentManager>,
        torrent: Arc<crate::torrent::meta::TorrentState>,
        disk: Arc<DiskManager>,
        root: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(tag: &str, content: &[u8]) -> Fixture {
        fixture_in_pieces(tag, content, content.len())
    }

    fn fixture_in_pieces(tag: &str, content: &[u8], piece_len: usize) -> Fixture {
        fixture_from(
            tag,
            torrent_bytes_in_pieces("payload.bin", content, piece_len),
        )
    }

    /// A torrent of several files (`multi/<name>`), pieces over their
    /// concatenation, with the real hashes.
    fn multi_torrent_bytes(files: &[(&str, &[u8])], piece_len: usize) -> Vec<u8> {
        let all: Vec<u8> = files.iter().flat_map(|(_, c)| c.iter().copied()).collect();
        let mut info = Vec::new();
        info.extend_from_slice(b"d5:filesl");
        for (name, c) in files {
            info.extend_from_slice(
                format!("d6:lengthi{}e4:pathl{}:{name}ee", c.len(), name.len()).as_bytes(),
            );
        }
        info.extend_from_slice(b"e4:name5:multi");
        let hashes: Vec<u8> = all.chunks(piece_len).flat_map(sha1_of).collect();
        info.extend_from_slice(
            format!("12:piece lengthi{piece_len}e6:pieces{}:", hashes.len()).as_bytes(),
        );
        info.extend_from_slice(&hashes);
        info.push(b'e');
        let mut out = b"d4:info".to_vec();
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn fixture_from(tag: &str, torrent_file: Vec<u8>) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "typhon-disk-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let data = root.join("data");
        let resume = root.join("resume");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();

        let disk = Arc::new(DiskManager::new(16));
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            disk.clone(),
        ));

        // ⭐ The piece hash table is NOT kept in RAM -- 20 bytes per piece over
        // a 300k library is the whole point of releasing it -- so `piece_hash`
        // reloads the metainfo through `blob_source` on demand. A manager with
        // none refuses to verify, which is the right call and makes every
        // write look like a hash mismatch. Set before the add: the state
        // copies the source when it is built.
        let blob = torrent_file.clone();
        mgr.set_blob_source(Arc::new(move |_hash: &str| Some(blob.clone())));

        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_file, data.to_string_lossy().as_ref(), true, false)
            .expect("the fixture torrent parses");
        let torrent = mgr.get(&ih).expect("just added");
        Fixture {
            _mgr: mgr,
            torrent,
            disk,
            root,
        }
    }

    /// `set_preallocate` is a process-wide setting: the tests that flip it run
    /// one at a time and put it back.
    async fn prealloc_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        L.lock().await
    }

    struct PreallocOn;
    impl PreallocOn {
        fn new() -> Self {
            set_preallocate(true);
            PreallocOn
        }
    }
    impl Drop for PreallocOn {
        fn drop(&mut self) {
            set_preallocate(false);
        }
    }

    fn pattern(seed: u8, n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
            .collect()
    }

    /// The length of the file and the bytes the file system holds for it:
    /// `blocks() * 512` on Unix, the allocation size on Windows.
    fn len_and_reserved(path: &std::path::Path) -> (u64, u64) {
        let m = std::fs::metadata(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            (m.len(), m.blocks() * 512)
        }
        #[cfg(windows)]
        {
            let f = File::open(path).unwrap();
            (m.len(), win_allocated(&f).unwrap())
        }
    }

    /// ⭐ With `preallocate`, the first piece written of a torrent reserves
    /// every wanted file at its declared size -- the one it lands in and the
    /// ones it does not touch -- without a byte of zeros being written.
    #[tokio::test]
    async fn preallocate_reserves_every_wanted_file_at_the_first_piece() {
        let _l = prealloc_lock().await;
        let _on = PreallocOn::new();
        let a = pattern(11, 12288);
        let b = pattern(23, 8192);
        let f = fixture_from(
            "prealloc-on",
            multi_torrent_bytes(&[("a.bin", &a), ("b.bin", &b)], 4096),
        );
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, a[..4096].to_vec())
            .await
            .unwrap());
        for (name, want) in [("a.bin", 12288u64), ("b.bin", 8192)] {
            let (len, reserved) = len_and_reserved(&f.root.join("data/multi").join(name));
            assert_eq!(len, want, "{name} must have its declared size");
            assert!(
                reserved >= want,
                "{name}: {reserved} bytes allocated, {want} declared"
            );
        }
        // The piece itself is intact.
        let back = f.disk.read_block(&f.torrent, 0, 0, 4096).await.unwrap();
        assert_eq!(&back[..], &a[..4096]);
    }

    /// Off (the default): the file is as long as what was written.
    #[tokio::test]
    async fn without_preallocate_a_file_is_as_long_as_what_was_written() {
        let _l = prealloc_lock().await;
        set_preallocate(false);
        let a = pattern(31, 12288);
        let b = pattern(41, 8192);
        let f = fixture_from(
            "prealloc-off",
            multi_torrent_bytes(&[("a.bin", &a), ("b.bin", &b)], 4096),
        );
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, a[..4096].to_vec())
            .await
            .unwrap());
        assert_eq!(len_and_reserved(&f.root.join("data/multi/a.bin")).0, 4096);
        assert!(!f.root.join("data/multi/b.bin").exists());
    }

    /// A file at priority 0 is never reserved whole, even when a piece that
    /// straddles it writes a few bytes into it; given a priority again, the
    /// next write reserves it.
    #[tokio::test]
    async fn preallocate_skips_a_file_at_priority_zero_until_it_is_wanted() {
        use crate::torrent::piece_picker::DownloadOrder;
        let _l = prealloc_lock().await;
        let _on = PreallocOn::new();
        let a = pattern(51, 6000);
        let b = pattern(61, 6000);
        let f = fixture_from(
            "prealloc-prio",
            multi_torrent_bytes(&[("a.bin", &a), ("b.bin", &b)], 4096),
        );
        f.torrent.set_download_order(DownloadOrder {
            file_prio: vec![1, 0],
            ..Default::default()
        });
        let all: Vec<u8> = a.iter().chain(b.iter()).copied().collect();
        // Piece 1 = a[4096..] + b[..2192]: straddles the two files.
        assert!(f
            .disk
            .write_piece(&f.torrent, 1, all[4096..8192].to_vec())
            .await
            .unwrap());
        let (la, ra) = len_and_reserved(&f.root.join("data/multi/a.bin"));
        assert_eq!(la, 6000);
        assert!(ra >= 6000);
        assert_eq!(
            len_and_reserved(&f.root.join("data/multi/b.bin")).0,
            2192,
            "the unwanted file holds only what was written, not its declared size"
        );
        // Wanted after all: the next piece written reserves it.
        f.torrent.set_download_order(DownloadOrder::default());
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, all[..4096].to_vec())
            .await
            .unwrap());
        let (lb, rb) = len_and_reserved(&f.root.join("data/multi/b.bin"));
        assert_eq!(lb, 6000, "b.bin is reserved once it is wanted");
        assert!(rb >= 6000);
    }

    /// A torrent that is removed with its files and added again at the same
    /// path (same process) is a new state: it is reserved again, whatever was
    /// remembered of the old one.
    #[tokio::test]
    async fn preallocate_reserves_again_a_torrent_added_anew() {
        let _l = prealloc_lock().await;
        let _on = PreallocOn::new();
        let a = pattern(71, 12288);
        let b = pattern(83, 8192);
        let torrent_file = multi_torrent_bytes(&[("a.bin", &a), ("b.bin", &b)], 4096);
        let f = fixture_from("prealloc-again", torrent_file.clone());
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, a[..4096].to_vec())
            .await
            .unwrap());
        let a_path = f.root.join("data/multi/a.bin");
        let b_path = f.root.join("data/multi/b.bin");
        assert_eq!(len_and_reserved(&b_path).0, 8192);

        // Removed with its files: the handles go, the folder goes.
        evict_fds(&[a_path.clone(), b_path.clone()]);
        std::fs::remove_dir_all(f.root.join("data/multi")).unwrap();

        // Added again, same info-hash, same save path: a new state.
        let resume = f.root.join("resume2");
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(crate::torrent::TorrentManager::new(
            f.root.join("data").to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            f.disk.clone(),
        ));
        let blob = torrent_file.clone();
        mgr.set_blob_source(Arc::new(move |_hash: &str| Some(blob.clone())));
        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_file,
                f.root.join("data").to_string_lossy().as_ref(),
                true,
                false,
            )
            .expect("the torrent is added again");
        let again = mgr.get(&ih).expect("just added");
        assert!(!Arc::ptr_eq(&again, &f.torrent), "a new state");
        assert!(f
            .disk
            .write_piece(&again, 0, a[..4096].to_vec())
            .await
            .unwrap());
        for (path, want) in [(&a_path, 12288u64), (&b_path, 8192)] {
            let (len, reserved) = len_and_reserved(path);
            assert_eq!(len, want, "{path:?} is reserved again");
            assert!(reserved >= want);
        }
    }

    /// ⭐ The reservation outlives the handle. The descriptors are closed
    /// (evicted, as the cache does on its own) and the files read again from
    /// the disk: each still has its declared size and the space is still
    /// taken. A reservation that lives only while a handle is open (Windows
    /// gives back an allocation past the end of file when the last handle
    /// closes) fails here, and is lost without a sound in the daemon, which
    /// reserves once per torrent and per process.
    #[tokio::test]
    async fn preallocate_survives_closing_every_handle() {
        let _l = prealloc_lock().await;
        let _on = PreallocOn::new();
        let a = pattern(91, 12288);
        let b = pattern(97, 8192);
        let f = fixture_from(
            "prealloc-closed",
            multi_torrent_bytes(&[("a.bin", &a), ("b.bin", &b)], 4096),
        );
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, a[..4096].to_vec())
            .await
            .unwrap());
        let a_path = f.root.join("data/multi/a.bin");
        let b_path = f.root.join("data/multi/b.bin");
        // Closed, and not added again: nothing reserves a second time.
        evict_fds(&[a_path.clone(), b_path.clone()]);
        for (path, want) in [(&a_path, 12288u64), (&b_path, 8192)] {
            let (len, reserved) = len_and_reserved(path);
            assert_eq!(len, want, "{path:?} keeps its declared size once closed");
            assert!(
                reserved >= want,
                "{path:?}: {reserved} bytes allocated once closed, {want} declared"
            );
        }
    }

    /// ⭐ A seed-only torrent writes nothing: the piece is refused with an
    /// error before anything touches the disk -- no file, no directory, no
    /// preallocation.
    #[tokio::test]
    async fn a_seed_only_torrent_refuses_every_write() {
        let _l = prealloc_lock().await;
        let _on = PreallocOn::new();
        let content = vec![7u8; 4096];
        let f = fixture("seedonly-write", &content);
        f.torrent
            .seed_only
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let r = f.disk.write_piece(&f.torrent, 0, content.clone()).await;
        assert!(r.is_err(), "refused with an error, not a verdict: {r:?}");
        assert!(
            !f.root.join("data/payload.bin").exists(),
            "no file was created"
        );
        assert_eq!(f.root.join("data").read_dir().unwrap().count(), 0);

        f.torrent
            .seed_only
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(f.disk.write_piece(&f.torrent, 0, content).await.unwrap());
        assert!(f.root.join("data/payload.bin").exists());
    }

    /// ⭐ A piece is checked against its hash BEFORE it lands. Accepting one
    /// that does not match is how a library ends up serving bytes it cannot
    /// prove, and every later read of it fails somewhere else.
    #[tokio::test]
    async fn a_piece_that_does_not_match_its_hash_is_refused() {
        let content = vec![7u8; 4096];
        let f = fixture("badhash", &content);
        let ok = f
            .disk
            .write_piece(&f.torrent, 0, vec![9u8; 4096])
            .await
            .expect("a mismatch is a verdict, not a failure");
        assert!(!ok, "the piece must be refused");
    }

    #[tokio::test]
    async fn a_piece_that_matches_its_hash_is_accepted_and_readable() {
        let content = vec![7u8; 4096];
        let f = fixture("goodhash", &content);
        let ok = f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap();
        assert!(ok, "the piece hashes correctly and must be kept");

        let back = f
            .disk
            .read_block(&f.torrent, 0, 0, content.len() as u32)
            .await
            .unwrap();
        assert_eq!(
            &back[..],
            &content[..],
            "what was written is what comes back"
        );
    }

    /// ⭐ A read that starts past the end of the torrent is an error, not a
    /// subtraction that wraps into a read of gigabytes.
    #[tokio::test]
    async fn a_read_starting_past_the_end_is_an_error() {
        let content = vec![7u8; 4096];
        let f = fixture("pastend", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        // 4096-byte torrent: offset 3 MiB is far beyond its only cache block.
        let r = f.disk.read_block(&f.torrent, 0, 3 << 20, 16).await;
        assert!(
            r.is_err(),
            "read past the end returned {:?}",
            r.map(|b| b.len())
        );
        // A read inside it still works.
        let back = f.disk.read_block(&f.torrent, 0, 100, 16).await.unwrap();
        assert_eq!(&back[..], &content[100..116]);
    }

    /// A folder that cannot be made is the error reported, not the open that
    /// fails after it for a reason it no longer says.
    #[tokio::test]
    async fn a_folder_that_cannot_be_made_is_the_error() {
        let content = vec![7u8; 4096];
        let f = fixture("nofolder", &content);
        let blocker = f.root.join("not-a-folder");
        std::fs::write(&blocker, b"x").unwrap();
        *f.torrent.save_path.write() = blocker.join("sub");
        let e = f
            .disk
            .write_piece(&f.torrent, 0, content)
            .await
            .unwrap_err();
        assert!(e.contains("create folder"), "{e}");
    }

    /// ⭐ The files a piece went to are synced before the state saying we
    /// hold it is written, so a power cut cannot leave a piece marked held
    /// whose bytes never reached the disk. Each file once per sweep.
    #[tokio::test]
    async fn written_files_are_synced_before_the_state_is_saved() {
        let content = vec![7u8; 4096];
        let f = fixture("sync", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        assert_eq!(f.disk.sync_written().synced, 1, "the one file written");
        assert_eq!(f.disk.sync_written().synced, 0, "nothing new since");
        assert!(f.disk.write_piece(&f.torrent, 0, content).await.unwrap());
        f._mgr.save_all_resume();
        assert_eq!(f.disk.sync_written().synced, 0, "the sweep synced it first");
    }

    /// A completion syncs its own torrent's files, not every file every
    /// torrent wrote since the last sweep.
    #[tokio::test]
    async fn a_completion_syncs_only_its_own_torrent() {
        let content = vec![7u8; 4096];
        let f = fixture("synconly", &content);
        assert!(f.disk.write_piece(&f.torrent, 0, content).await.unwrap());
        assert_eq!(
            f.disk.sync_torrent(&[0u8; 20]).synced,
            0,
            "another torrent wrote nothing"
        );
        assert_eq!(
            f.disk.sync_torrent(&f.torrent.info_hash).synced,
            1,
            "its one file"
        );
        assert_eq!(f.disk.sync_written().synced, 0, "and it left the set");
    }

    /// A block is a window into a piece, not the whole of it.
    #[tokio::test]
    async fn a_block_reads_the_window_it_asked_for() {
        let mut content = vec![0u8; 4096];
        for (i, b) in content.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let f = fixture("window", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());

        let back = f.disk.read_block(&f.torrent, 0, 1000, 256).await.unwrap();
        assert_eq!(&back[..], &content[1000..1256]);
    }

    /// Reading the same block twice must answer the same bytes: the second
    /// read comes out of the cache, and a cache that answers differently from
    /// the disk is worse than no cache.
    #[tokio::test]
    async fn a_cached_read_answers_what_the_disk_holds() {
        let content = vec![3u8; 4096];
        let f = fixture("cache", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());

        let first = f.disk.read_block(&f.torrent, 0, 0, 512).await.unwrap();
        let second = f.disk.read_block(&f.torrent, 0, 0, 512).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(&first[..], &content[..512]);
    }

    /// ⭐ The trade-off of answering without a copy: the 16 KiB answer is a
    /// view into the 128 KiB slice, so while the answer is alive the whole
    /// slice is, even if the cache has let it go. How long is "alive"? Until
    /// the codec writes the message into the socket buffer, which copies it:
    /// `framed.send` encodes at once. This pins both ends.
    #[tokio::test]
    async fn an_answer_shares_its_slice_until_the_codec_copies_it_out() {
        use tokio_util::codec::Encoder;
        let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture("share", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let key = readcache::Key {
            info_hash: f.torrent.info_hash,
            start: 0,
        };

        let answer = f
            .disk
            .read_block(&f.torrent, 0, 16384, 16384)
            .await
            .unwrap();
        assert_eq!(&answer[..], &content[16384..32768]);
        let slice = f.disk.read_cache().holds(&key).expect("the slice is kept");
        assert_eq!(
            answer.as_ptr(),
            slice[16384..].as_ptr(),
            "no copy: the answer points into the slice"
        );
        drop(slice);
        assert!(
            !f.disk.read_cache().holds_alone(&key),
            "the answer holds it too"
        );

        // The cache lets it go; the answer still reads right, and is now the
        // only thing keeping 128 KiB alive for its 16. This is the cost.
        let kept = answer.clone();
        f.disk.read_cache().set_budget(0);
        assert_eq!(f.disk.read_cache().bytes(), 0);
        assert_eq!(&kept[..], &content[16384..32768]);
        drop(kept);
        f.disk.read_cache().set_budget(readcache::DEFAULT_MB << 20);

        // And the codec ends it: the message is copied into the write
        // buffer and the answer dropped with it.
        let again = f.disk.read_block(&f.torrent, 0, 0, 16384).await.unwrap();
        drop(answer);
        let mut out = bytes::BytesMut::new();
        crate::wire::codec::BtCodec::new()
            .encode(
                crate::peer::message::Message::Piece {
                    index: 0,
                    begin: 0,
                    data: again,
                },
                &mut out,
            )
            .unwrap();
        assert_eq!(&out[13..], &content[..16384]);
        assert!(
            f.disk.read_cache().holds_alone(&key),
            "once encoded, nothing but the cache refers to the slice"
        );
    }

    /// ⭐ Eight requests for the eight 16 KiB parts of one cold slice, all at once:
    /// one disk read, and each gets its own part. Holding the descriptor
    /// cache parks the first read until the others are waiting on it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn requests_for_one_cold_slice_share_one_disk_read() {
        let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture("oneread", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let path = f.root.join("data").join("payload.bin");
        served_reads().lock().remove(&path);

        let held = fd_cache().lock();
        let tasks: Vec<_> = (0..8u32)
            .map(|i| {
                let (disk, t) = (f.disk.clone(), f.torrent.clone());
                tokio::spawn(async move { (i, disk.read_block(&t, 0, i * 16384, 16384).await) })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(held);
        for t in tasks {
            let (i, got) = t.await.unwrap();
            let at = (i * 16384) as usize;
            assert_eq!(&got.unwrap()[..], &content[at..at + 16384], "part {i}");
        }
        assert_eq!(
            served_reads().lock().get(&path),
            Some(&1),
            "one read for the slice"
        );

        // The next slice is another read; the first is answered from memory.
        f.disk
            .read_block(&f.torrent, 0, 131072, 16384)
            .await
            .unwrap();
        f.disk.read_block(&f.torrent, 0, 0, 16384).await.unwrap();
        assert_eq!(served_reads().lock().get(&path), Some(&2));
    }

    /// What `serve_block` does for one buffered request: note it on the
    /// walk, read it, and read ahead if it followed the last one.
    async fn serve(
        f: &Fixture,
        walk: &mut readcache::Walk,
        piece: u32,
        offset: u32,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let in_order = walk.step(piece, offset, 16384);
        f.disk
            .read_block(&f.torrent, piece, offset, 16384)
            .await
            .unwrap();
        if in_order {
            f.disk.prefetch_next(walk, &f.torrent, piece, offset)
        } else {
            None
        }
    }

    fn slice_key(f: &Fixture, start: u64) -> readcache::Key {
        readcache::Key {
            info_hash: f.torrent.info_hash,
            start,
        }
    }

    /// ⭐ Issue #123. A peer walking a piece in order: by the time it asks
    /// for the next slice, that slice was read ahead and is answered from
    /// memory, so the walk costs one disk read per slice and none of them
    /// waits for the request. Without the read-ahead the third read below
    /// is the request's own, on its path.
    #[tokio::test]
    async fn a_peer_walking_a_piece_in_order_finds_the_next_slice_read_ahead() {
        let content: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture("readahead", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let path = f.root.join("data").join("payload.bin");
        served_reads().lock().remove(&path);
        let issued = readcache::PREFETCH_ISSUED.load(Ordering::Relaxed);

        let mut walk = readcache::Walk::default();
        assert!(
            serve(&f, &mut walk, 0, 0).await.is_none(),
            "one request is no walk"
        );
        serve(&f, &mut walk, 0, 16384)
            .await
            .expect("the second in order reads ahead")
            .await
            .unwrap();
        assert_eq!(served_reads().lock().get(&path), Some(&2));
        assert!(f
            .disk
            .read_cache()
            .holds_unused_prefetch(&slice_key(&f, SLICE)));
        assert!(readcache::PREFETCH_ISSUED.load(Ordering::Relaxed) > issued);

        // The rest of the slice asks for nothing more.
        for i in 2..8 {
            assert!(
                serve(&f, &mut walk, 0, i * 16384).await.is_none(),
                "block {i}"
            );
        }
        // Into the next slice: from memory, and the one after is read ahead.
        let ahead = serve(&f, &mut walk, 0, 8 * 16384).await;
        assert!(!f
            .disk
            .read_cache()
            .holds_unused_prefetch(&slice_key(&f, SLICE)));
        ahead.expect("the slice after").await.unwrap();
        assert_eq!(
            served_reads().lock().get(&path),
            Some(&3),
            "slice 1 came out of memory; slice 2 was read ahead"
        );
        assert_eq!(
            f.disk.prefetches.load(Ordering::Relaxed),
            0,
            "its place is given back"
        );
    }

    /// Nothing is read ahead for requests out of order, nor past the end of
    /// the piece being walked: the peer may want any piece next.
    #[tokio::test]
    async fn nothing_is_read_ahead_out_of_order_or_past_the_piece() {
        let content: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture_in_pieces("readahead-edge", &content, 128 * 1024);
        for (i, piece) in content.chunks(128 * 1024).enumerate() {
            assert!(f
                .disk
                .write_piece(&f.torrent, i as u32, piece.to_vec())
                .await
                .unwrap());
        }
        let mut walk = readcache::Walk::default();
        assert!(!walk.step(0, 32768, 16384));
        assert!(!walk.step(0, 0, 16384), "backwards");
        assert!(!walk.step(1, 16384, 16384), "another piece");
        assert!(walk.step(1, 32768, 16384));

        // A piece of one slice: walking piece 0 never reads past it.
        let mut walk = readcache::Walk::default();
        for i in 0..8 {
            assert!(
                serve(&f, &mut walk, 0, i * 16384).await.is_none(),
                "block {i}"
            );
        }
        assert!(f.disk.read_cache().holds(&slice_key(&f, SLICE)).is_none());
    }

    /// A slice already held is not read again ahead of time.
    #[tokio::test]
    async fn a_held_slice_is_not_read_ahead() {
        let content: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture("readahead-held", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let path = f.root.join("data").join("payload.bin");
        f.disk
            .read_block(&f.torrent, 0, 131072, 16384)
            .await
            .unwrap();
        served_reads().lock().remove(&path);

        let mut walk = readcache::Walk::default();
        serve(&f, &mut walk, 0, 0).await;
        assert!(serve(&f, &mut walk, 0, 16384).await.is_none());
        assert_eq!(served_reads().lock().get(&path), Some(&1), "slice 0 only");
    }

    /// The engine has at most `PREFETCH_MAX` read-aheads under way. One
    /// refused for that is asked again by the peer's next request.
    #[tokio::test]
    async fn read_ahead_waits_for_a_place_under_the_engine_bound() {
        let content: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture("readahead-bound", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        f.disk
            .prefetches
            .store(readcache::PREFETCH_MAX, Ordering::Relaxed);
        let mut walk = readcache::Walk::default();
        serve(&f, &mut walk, 0, 0).await;
        assert!(serve(&f, &mut walk, 0, 16384).await.is_none(), "no place");
        assert!(f.disk.read_cache().holds(&slice_key(&f, SLICE)).is_none());

        f.disk
            .prefetches
            .store(readcache::PREFETCH_MAX - 1, Ordering::Relaxed);
        serve(&f, &mut walk, 0, 32768)
            .await
            .expect("a place freed")
            .await
            .unwrap();
        assert!(f.disk.read_cache().holds(&slice_key(&f, SLICE)).is_some());
        assert_eq!(
            f.disk.prefetches.load(Ordering::Relaxed),
            readcache::PREFETCH_MAX - 1
        );
    }

    /// A request for a slice still being read ahead waits on that read: one
    /// disk read, and the read-ahead counts as used.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_request_joins_the_read_ahead_of_its_slice() {
        let content: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture("readahead-join", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let path = f.root.join("data").join("payload.bin");
        let mut walk = readcache::Walk::default();
        serve(&f, &mut walk, 0, 0).await;
        served_reads().lock().remove(&path);

        // The read-ahead starts and blocks on the descriptor cache.
        let held = fd_cache().lock();
        let ahead = serve(&f, &mut walk, 0, 16384).await.expect("read ahead");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (disk, t) = (f.disk.clone(), f.torrent.clone());
        let request = tokio::spawn(async move { disk.read_block(&t, 0, 131072, 16384).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(held);
        ahead.await.unwrap();
        assert_eq!(
            &request.await.unwrap().unwrap()[..],
            &content[131072..147456]
        );
        assert_eq!(served_reads().lock().get(&path), Some(&1), "one read");
        assert!(!f
            .disk
            .read_cache()
            .holds_unused_prefetch(&slice_key(&f, SLICE)));
    }

    /// The request running a slice's read can be dropped (its peer went
    /// away): one waiting on it then reads the slice itself, never hangs.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_waiter_takes_over_the_read_of_a_dropped_request() {
        let content: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture("takeover", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        // Cold: what the write left cached is dropped.
        f.disk.read_cache().set_budget(0);
        f.disk.read_cache().set_budget(readcache::DEFAULT_MB << 20);

        let held = fd_cache().lock();
        let (disk, t) = (f.disk.clone(), f.torrent.clone());
        let first = tokio::spawn(async move { disk.read_block(&t, 0, 0, 16384).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (disk, t) = (f.disk.clone(), f.torrent.clone());
        let second = tokio::spawn(async move { disk.read_block(&t, 0, 16384, 16384).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        first.abort();
        drop(held);
        let got = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the waiter must not hang")
            .unwrap()
            .unwrap();
        assert_eq!(&got[..], &content[16384..32768]);
    }

    /// `read_cache_mb = 0`: no cache, every request read alone, all correct.
    #[tokio::test]
    async fn with_no_budget_every_request_still_reads_right() {
        let content: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();
        let f = fixture("nobudget", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        f.disk.read_cache().set_budget(0);
        for i in 0..8u32 {
            let at = (i * 16384) as usize;
            let got = f
                .disk
                .read_block(&f.torrent, 0, i * 16384, 16384)
                .await
                .unwrap();
            assert_eq!(&got[..], &content[at..at + 16384]);
        }
        assert_eq!(f.disk.read_cache().slices(), 0);
    }

    /// ⭐ A block whose first page is cached and whose last is not is cold:
    /// sendfile on it would wait on the disk inside an async worker. The old
    /// probe asked the first page only and let it through. Linux only (the
    /// probe is preadv2); the tail is dropped from the page cache with
    /// posix_fadvise, which a filesystem may ignore (tmpfs): then this says
    /// so and checks nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_block_with_a_cold_tail_is_not_resident() {
        use std::io::Write;
        use std::os::unix::io::AsRawFd;
        let dir = std::env::temp_dir().join(format!("typhon-resident-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin");
        let mut f = File::create(&path).unwrap();
        f.write_all(&vec![7u8; 1 << 20]).unwrap();
        f.sync_all().unwrap();
        drop(f);
        let f = File::open(&path).unwrap();
        // Drop the whole file from the page cache (a range of pages written
        // a moment ago can refuse to go; the whole file does not), then read
        // the first half back: head cached, tail cold.
        // SAFETY: a valid open descriptor, and plain integers.
        let rc = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        assert_eq!(rc, 0);
        // No readahead on this descriptor, or reading the head brings the
        // tail in behind it.
        // SAFETY: as above.
        let rc = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_RANDOM) };
        assert_eq!(rc, 0);
        let mut b = vec![0u8; 32 << 10];
        pread_exact(&f, &mut b, 0).unwrap();
        let skip = |why: &str| {
            eprintln!("{why}: nothing checked");
            let _ = std::fs::remove_dir_all(&dir);
        };
        if !page_resident(&f, 0) {
            // overlayfs, for one, answers RWF_NOWAIT with EOPNOTSUPP: every
            // block is then cold and served by the buffered path.
            return skip("this filesystem does not answer RWF_NOWAIT");
        }
        // Readahead may have brought the tail in with the head.
        if page_resident(&f, 36 << 10) || page_resident(&f, 900 << 10) {
            return skip("the file stayed cached past what was read");
        }
        assert!(
            is_block_resident(&f, 0, 16 << 10),
            "a block all in the head"
        );
        // 24..40 KiB: head page cached, last page cold. The old probe, first
        // page only, said yes.
        let first_page_only = page_resident(&f, 24 << 10);
        let both_ends = is_block_resident(&f, 24 << 10, 16 << 10);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(first_page_only, "the head page is cached");
        assert!(!both_ends, "the tail is cold: not resident");
    }

    /// ⭐ Writing a piece must INVALIDATE the blocks cached over it, or a
    /// re-checked torrent keeps serving the bytes it has just replaced.
    #[tokio::test]
    async fn rewriting_a_piece_invalidates_what_was_cached_over_it() {
        let content = vec![1u8; 4096];
        let f = fixture("invalidate", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let before = f.disk.read_block(&f.torrent, 0, 0, 16).await.unwrap();
        assert_eq!(&before[..], &[1u8; 16][..]);

        // The same bytes again: the write must still drop the cached block
        // rather than leave a stale one behind.
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let after = f.disk.read_block(&f.torrent, 0, 0, 16).await.unwrap();
        assert_eq!(&after[..], &before[..]);
    }

    /// A read past the end of the torrent is an error, never a short buffer a
    /// peer would take for real data.
    #[tokio::test]
    async fn a_read_past_the_end_is_an_error_not_a_short_read() {
        let content = vec![5u8; 1024];
        let f = fixture("past", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        assert!(f.disk.read_block(&f.torrent, 0, 1000, 4096).await.is_err());
        assert!(f.disk.read_block(&f.torrent, 99, 0, 16).await.is_err());
    }

    /// Reading a torrent whose data was never written is an error, not zeroes.
    /// Zeroes would be served to a peer as if they were the file.
    #[tokio::test]
    async fn reading_data_that_was_never_written_is_an_error() {
        let content = vec![5u8; 1024];
        let f = fixture("absent", &content);
        assert!(f.disk.read_block(&f.torrent, 0, 0, 16).await.is_err());
    }

    /// The zero-copy path hands back a descriptor and an offset for a request
    /// that sits inside ONE file; a request spanning two files has no single
    /// descriptor and must decline rather than serve the wrong one.
    #[tokio::test]
    async fn the_zero_copy_path_answers_for_a_request_inside_one_file() {
        let content = vec![2u8; 4096];
        let f = fixture("sendfile", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let got = f.disk.block_file(&f.torrent, 0, 0, 512);
        assert!(got.is_some(), "a single-file torrent has one descriptor");
        let (_, offset) = got.unwrap();
        assert_eq!(offset, 0);
    }

    /// ⭐ A read that goes to disk while a neighbouring piece is being written
    /// may read that piece's old bytes. It can serve its own piece from them,
    /// but it must not cache them over the piece the write has just landed,
    /// or that piece is served wrong until the block expires.
    ///
    /// Holding the descriptor cache parks the reader after its cache miss and
    /// the writer after its invalidation; on release the reader usually reads
    /// first. A few rounds make that ordering all but certain.
    // The descriptor-cache lock is held across the awaits on purpose: that is how the
    // test parks the read and the write.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_racing_a_write_never_caches_what_the_write_replaced() {
        const PIECE: usize = 16 * 1024;
        let mut content = vec![1u8; PIECE];
        content.extend_from_slice(&[2u8; PIECE]);
        for round in 0..20 {
            let f = fixture_in_pieces(&format!("race{round}"), &content, PIECE);
            assert!(f
                .disk
                .write_piece(&f.torrent, 0, content[..PIECE].to_vec())
                .await
                .unwrap());
            // Full length on disk, so piece 1 reads back as zeros until written.
            std::fs::OpenOptions::new()
                .write(true)
                .open(f.root.join("data").join("payload.bin"))
                .unwrap()
                .set_len(content.len() as u64)
                .unwrap();

            let held = fd_cache().lock();
            let (disk, t) = (f.disk.clone(), f.torrent.clone());
            let reader = tokio::spawn(async move { disk.read_block(&t, 0, 0, 16).await });
            tokio::time::sleep(Duration::from_millis(30)).await;
            let (disk, t, piece) = (f.disk.clone(), f.torrent.clone(), content[PIECE..].to_vec());
            let writer = tokio::spawn(async move { disk.write_piece(&t, 1, piece).await });
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(held);
            assert_eq!(&reader.await.unwrap().unwrap()[..], &[1u8; 16][..]);
            assert!(writer.await.unwrap().unwrap());

            let got = f.disk.read_block(&f.torrent, 1, 0, 16).await.unwrap();
            assert_eq!(
                &got[..],
                &[2u8; 16][..],
                "round {round}: piece 1 as written"
            );
        }
    }

    fn holds_descriptor_for(path: &std::path::Path) -> bool {
        fd_cache().lock().contains(&path.to_path_buf())
    }

    /// ⭐ A torrent that leaves the engine with its data kept (a move, a
    /// graduation) gives back its descriptors: the move unlinks the old file
    /// next, and a cached descriptor would keep its blocks allocated.
    #[tokio::test]
    async fn a_torrent_removed_with_its_data_kept_lets_go_of_its_files() {
        let content = vec![4u8; 4096];
        let f = fixture("keepdata", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let path = f.root.join("data").join("payload.bin");
        assert!(
            holds_descriptor_for(&path),
            "the write cached its descriptor"
        );

        f._mgr.remove_torrent(&f.torrent.info_hash, true).unwrap();
        assert!(path.exists(), "the data stays");
        assert!(
            !holds_descriptor_for(&path),
            "but no descriptor holds it any more"
        );
    }

    /// Changing where a torrent lives drops the descriptors of where it was.
    #[tokio::test]
    async fn a_new_save_path_lets_go_of_the_old_files() {
        let content = vec![6u8; 4096];
        let f = fixture("relocate", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        let old = f.root.join("data").join("payload.bin");
        assert!(holds_descriptor_for(&old));

        let moved = f.root.join("moved");
        std::fs::create_dir_all(&moved).unwrap();
        f._mgr
            .set_save_path(&f.torrent.info_hash, moved.to_string_lossy().as_ref())
            .unwrap();
        assert!(
            !holds_descriptor_for(&old),
            "the old path is no longer this torrent's"
        );
    }

    /// A guarded file is compared, never written: gone, a piece creates it (it
    /// overwrites nothing); cut short, the piece is refused (writing would
    /// extend a file that is not this torrent's); whole and identical, the
    /// piece counts and the file is not touched.
    #[tokio::test]
    async fn a_guarded_file_is_compared_not_written() {
        let content: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let f = fixture_in_pieces("guarded", &content, 4096);
        let path = f.root.join("data").join("payload.bin");
        f.torrent
            .guard_files([std::path::PathBuf::from("payload.bin")].into());

        // Gone: written.
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), content);

        // Cut short: refused, the file as it was.
        std::fs::write(&path, &content[..100]).unwrap();
        let why = f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap_err();
        assert!(why.starts_with("refused to overwrite"), "{why}");
        assert_eq!(std::fs::read(&path).unwrap(), &content[..100]);

        // Whole and identical: accepted, and the file is not written (its
        // back-dated modification time stays).
        std::fs::write(&path, &content).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
    }

    /// A torrent that never went through the proof job -- what a restart
    /// leaves: no guard set -- whose file is hardlinked elsewhere (a library).
    /// Bytes that differ: the piece is refused and both names are untouched.
    /// Bytes that are the same: nothing is written, the piece counts.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hardlinked_file_is_compared_without_any_guard_set() {
        use std::os::unix::fs::MetadataExt;
        let content: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let stamp = |p: &std::path::Path| {
            let m = std::fs::metadata(p).unwrap();
            (
                m.ino(),
                m.nlink(),
                m.modified().unwrap(),
                std::fs::read(p).unwrap(),
            )
        };

        for same in [false, true] {
            let f = fixture_in_pieces(if same { "link-same" } else { "link-diff" }, &content, 4096);
            assert!(f.torrent.guarded().is_empty(), "no proof job ran");
            let path = f.root.join("data").join("payload.bin");
            let lib = f.root.join("library.bin");
            let mut on_disk = content.clone();
            if !same {
                on_disk[4095] ^= 0xff;
            }
            std::fs::write(&path, &on_disk).unwrap();
            std::fs::hard_link(&path, &lib).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
            let before = (stamp(&path), stamp(&lib));
            assert_eq!(before.0 .1, 2);

            let got = f.disk.write_piece(&f.torrent, 0, content.clone()).await;
            if same {
                assert!(got.unwrap(), "identical bytes: the piece counts");
            } else {
                let why = got.unwrap_err();
                assert!(why.starts_with("refused to overwrite"), "{why}");
            }
            assert_eq!((stamp(&path), stamp(&lib)), before, "a link was written");
        }
    }

    /// A normal download -- files that are new, or its own with one link and
    /// named by nobody -- compares and reads back nothing: the file is stat'ed
    /// once, whatever the number of pieces, and the claim probe is asked only
    /// for a file that was already there.
    #[tokio::test]
    async fn a_normal_download_compares_nothing() {
        let mut content = Vec::new();
        for i in 0..4u8 {
            content.extend(std::iter::repeat_n(i + 1, 4096));
        }
        let f = fixture_in_pieces("normal", &content, 4096);
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let a = asked.clone();
        f.disk.set_claim_probe(Arc::new(move |_| {
            a.fetch_add(1, Ordering::Relaxed);
            std::collections::HashSet::new()
        }));
        for (p, c) in content.chunks(4096).enumerate() {
            assert!(f
                .disk
                .write_piece(&f.torrent, p as u32, c.to_vec())
                .await
                .unwrap());
        }
        assert_eq!(asked.load(Ordering::Relaxed), 0, "a new file is not probed");
        assert_eq!(
            std::fs::read(f.root.join("data").join("payload.bin")).unwrap(),
            content
        );
        // The verdict was taken once, at the first piece.
        assert_eq!(f.torrent.write_verdicts().lock().files.len(), 1);

        // A resumed download: the file is there with one link, nobody names it.
        let g = fixture_in_pieces("resumed", &content, 4096);
        let b = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let b2 = b.clone();
        g.disk.set_claim_probe(Arc::new(move |_| {
            b2.fetch_add(1, Ordering::Relaxed);
            std::collections::HashSet::new()
        }));
        std::fs::write(g.root.join("data").join("payload.bin"), &content[..4096]).unwrap();
        for (p, c) in content.chunks(4096).enumerate() {
            assert!(g
                .disk
                .write_piece(&g.torrent, p as u32, c.to_vec())
                .await
                .unwrap());
        }
        assert_eq!(b.load(Ordering::Relaxed), 1, "asked once for the torrent");
    }

    /// A verdict taken without the means to ask is no verdict: with no claim
    /// probe yet, an existing single-link file is written as before but nothing
    /// is kept, and once the probe exists the next piece asks it.
    #[tokio::test]
    async fn no_verdict_is_kept_while_nobody_can_be_asked() {
        let content: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let f = fixture_in_pieces("noprobe", &content, 4096);
        let path = f.root.join("data").join("payload.bin");
        std::fs::write(&path, &content).unwrap();
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        assert!(
            f.torrent.write_verdicts().lock().files.is_empty(),
            "a guess was kept"
        );
        f.disk.set_claim_probe(Arc::new(|_| {
            [std::path::PathBuf::from("payload.bin")].into()
        }));
        let mut other = content.clone();
        other[7] ^= 1;
        std::fs::write(&path, &other).unwrap();
        let why = f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap_err();
        assert!(why.starts_with("refused to overwrite"), "{why}");
        assert_eq!(std::fs::read(&path).unwrap(), other);
    }

    /// The verdict belongs to a place: taken at one ("not compared": the file
    /// was the torrent's own), it is forgotten when the torrent moves, so a
    /// file of the same name that is already there, hardlinked, with a byte of
    /// difference, is not written over.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_move_forgets_what_was_found_at_the_old_place() {
        let content: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let f = fixture_in_pieces("moved", &content, 4096);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        assert_eq!(
            f.torrent
                .write_verdicts()
                .lock()
                .files
                .values()
                .collect::<Vec<_>>(),
            vec![&false]
        );

        let new = f.root.join("moved");
        std::fs::create_dir_all(&new).unwrap();
        let mut other = content.clone();
        other[100] ^= 0xff;
        std::fs::write(new.join("payload.bin"), &other).unwrap();
        std::fs::hard_link(new.join("payload.bin"), f.root.join("library.bin")).unwrap();
        f._mgr
            .set_save_path(&f.torrent.info_hash, &new.to_string_lossy())
            .unwrap();

        let why = f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap_err();
        assert!(why.starts_with("refused to overwrite"), "{why}");
        assert_eq!(std::fs::read(new.join("payload.bin")).unwrap(), other);
        assert_eq!(std::fs::read(f.root.join("library.bin")).unwrap(), other);
    }

    /// A file renamed onto one that is already there -- hardlinked, with a byte
    /// of difference -- is compared at its next piece, not written over: the
    /// rename drops what was found under the old name and is entered in the
    /// manager's placement log, which is what the claim index reads.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_renamed_file_is_compared_at_its_new_name() {
        let content: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let f = fixture_in_pieces("renamed", &content, 4096);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        assert_eq!(f.torrent.write_verdicts().lock().files.len(), 1);

        // The old name goes away, the new one is somebody else's file.
        std::fs::remove_file(f.root.join("data").join("payload.bin")).unwrap();
        let mut other = content.clone();
        other[100] ^= 0xff;
        std::fs::write(f.root.join("data").join("moved.bin"), &other).unwrap();
        std::fs::hard_link(
            f.root.join("data").join("moved.bin"),
            f.root.join("library.bin"),
        )
        .unwrap();
        let mark = f._mgr.placements();
        f._mgr
            .rename_file(&f.torrent.info_hash, "payload.bin", "moved.bin")
            .unwrap();
        assert_eq!(
            f._mgr.placed_since(mark),
            Some(vec![f.torrent.info_hash]),
            "the rename is not in the placement log"
        );

        let why = f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap_err();
        assert!(why.starts_with("refused to overwrite"), "{why}");
        assert_eq!(
            std::fs::read(f.root.join("data").join("moved.bin")).unwrap(),
            other
        );
        assert_eq!(std::fs::read(f.root.join("library.bin")).unwrap(), other);
    }

    fn four_piece_fixture(tag: &str) -> (Fixture, Vec<Vec<u8>>) {
        let mut content = Vec::new();
        for i in 0..4u8 {
            content.extend(std::iter::repeat_n(i + 1, 4096));
        }
        let chunks = content.chunks(4096).map(|c| c.to_vec()).collect();
        let f = fixture_in_pieces(tag, &content, 4096);
        // The fixture is added stopped; these are downloads under way.
        f.torrent.status.store(
            crate::torrent::meta::TorrentStatus::Downloading as u8,
            Ordering::Relaxed,
        );
        (f, chunks)
    }

    /// What `commit` does once a piece is down: written, then counted held.
    async fn download(f: &Fixture, piece: u32, data: &[u8]) {
        assert!(f
            .disk
            .write_piece(&f.torrent, piece, data.to_vec())
            .await
            .unwrap());
        f.torrent
            .picker
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .set_have(piece);
    }

    fn held(f: &Fixture) -> Vec<u32> {
        let p = f.torrent.picker.get().unwrap().lock().unwrap();
        (0..4).filter(|i| p.has_piece(*i)).collect()
    }

    fn saved_bitfield(f: &Fixture) -> String {
        let db = crate::torrent::statedb::StateDb::open(&f.root.join("state.db")).unwrap();
        db.load_all()
            .into_iter()
            .next()
            .expect("a record was saved")
            .bitfield
    }

    /// ⭐ A file that will not sync takes its pieces out of what we hold, and
    /// out of the record, before the record is written. Retrying the sync
    /// proves nothing: after a failed flush the kernel may drop the dirty
    /// pages and report the next one clean.
    #[tokio::test]
    async fn pieces_written_to_a_file_that_failed_to_sync_are_not_held() {
        let (f, chunks) = four_piece_fixture("syncfail");
        for (i, c) in chunks.iter().enumerate() {
            download(&f, i as u32, c).await;
        }
        assert_eq!(held(&f), vec![0, 1, 2, 3]);
        fail_sync_of(f.root.join("data").join("payload.bin"));

        f._mgr.save_all_resume();

        assert_eq!(
            held(&f),
            Vec::<u32>::new(),
            "nothing durable is vouched for"
        );
        assert_eq!(saved_bitfield(&f), "00", "and the record says so");
        let msg = f.torrent.error_msg.lock().unwrap().clone();
        assert!(msg.contains("sync"), "the torrent says why: {msg:?}");
        assert_eq!(
            f.torrent.status.load(Ordering::Relaxed),
            crate::torrent::meta::TorrentStatus::Error as u8,
            "and shows as an error"
        );
    }

    /// Pieces that reached the disk in an earlier, good sweep stay held; the
    /// failure takes only what was written since.
    #[tokio::test]
    async fn a_failed_sync_leaves_pieces_synced_before_it_alone() {
        let (f, chunks) = four_piece_fixture("syncpart");
        download(&f, 0, &chunks[0]).await;
        download(&f, 1, &chunks[1]).await;
        f._mgr.save_all_resume();
        assert_eq!(held(&f), vec![0, 1]);

        download(&f, 2, &chunks[2]).await;
        fail_sync_of(f.root.join("data").join("payload.bin"));
        f._mgr.save_all_resume();

        assert_eq!(held(&f), vec![0, 1], "only the piece written since is lost");
        assert_eq!(saved_bitfield(&f), "c0");
    }

    /// The completion record is written after the same check: a download whose
    /// last flush failed is not recorded as whole.
    #[tokio::test]
    async fn a_completion_whose_sync_failed_is_not_recorded_whole() {
        let (f, chunks) = four_piece_fixture("synccomplete");
        for (i, c) in chunks.iter().enumerate() {
            download(&f, i as u32, c).await;
        }
        f.torrent.status.store(
            crate::torrent::meta::TorrentStatus::Seeding as u8,
            Ordering::Relaxed,
        );
        fail_sync_of(f.root.join("data").join("payload.bin"));

        f._mgr.on_completed(&f.torrent.info_hash);

        assert_eq!(held(&f), Vec::<u32>::new());
        assert_eq!(saved_bitfield(&f), "00");
        assert_ne!(
            f.torrent.status.load(Ordering::Relaxed),
            crate::torrent::meta::TorrentStatus::Seeding as u8,
            "no longer announced as a seed"
        );
    }

    /// ⭐ A read that fails on the disk for a peer takes the piece it was for
    /// out of what we hold -- it is not served again, nor offered -- and the
    /// torrent says why. The other pieces of the file stay.
    #[tokio::test]
    async fn a_piece_that_cannot_be_read_is_no_longer_held() {
        let (f, chunks) = four_piece_fixture("readfail");
        for (i, c) in chunks.iter().enumerate() {
            download(&f, i as u32, c).await;
        }
        f.torrent.status.store(
            crate::torrent::meta::TorrentStatus::Seeding as u8,
            Ordering::Relaxed,
        );
        f._mgr.save_all_resume();
        fail_read_of(f.root.join("data").join("payload.bin"));

        let r = f.disk.read_block(&f.torrent, 1, 0, 16).await;
        assert!(r.is_err(), "the read fails");

        assert_eq!(
            held(&f),
            vec![0, 2, 3],
            "only the unreadable piece is given up"
        );
        let msg = f.torrent.error_msg.lock().unwrap().clone();
        assert!(msg.contains("piece 1"), "the torrent says which: {msg:?}");
        assert_ne!(
            f.torrent.status.load(Ordering::Relaxed),
            crate::torrent::meta::TorrentStatus::Seeding as u8,
            "and is no longer presented as whole"
        );
        let advertised = f.torrent.have_bitfield();
        assert_eq!(advertised[0], 0b1011_0000, "nor is it offered to peers");

        f._mgr.save_all_resume();
        assert_eq!(saved_bitfield(&f), "b0", "nor saved as held");
    }

    /// The saved bitfield, if a record has been written at all.
    fn saved_bitfield_opt(f: &Fixture) -> Option<String> {
        let db = crate::torrent::statedb::StateDb::open(&f.root.join("state.db")).unwrap();
        db.load_all().into_iter().next().map(|r| r.bitfield)
    }

    /// The real session start, as the engine binary and Hydra call it: it is
    /// what spawns the resume sweeps. Nothing is reached on the network
    /// (DHT, LSD and webseed off, a free local port).
    async fn start_session(f: &Fixture, extra: &str) {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg: crate::config::EngineConfig = serde_json::from_str(&format!(
            r#"{{"listen_port":{port},"dht_enabled":false,"lsd_enabled":false,"enable_webseed":false{extra}}}"#
        ))
        .unwrap();
        crate::session::start(
            f._mgr.clone(),
            f.disk.clone(),
            &cfg,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
    }

    /// Let `secs` of (paused) time go by, then give the blocking pool the real
    /// time to finish a pass that was started.
    async fn let_pass(secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(10));
            tokio::task::yield_now().await;
        }
    }

    /// ⭐ A download that moves is in the state database within a minute, not
    /// five: a kill -9 costs at most the last minute of pieces.
    #[tokio::test(start_paused = true)]
    async fn a_download_that_moved_is_saved_within_a_minute() {
        let (f, chunks) = four_piece_fixture("fastsave");
        start_session(&f, "").await;
        download(&f, 0, &chunks[0]).await;
        download(&f, 1, &chunks[1]).await;

        let_pass(61).await;

        assert_eq!(
            saved_bitfield_opt(&f).as_deref(),
            Some("c0"),
            "two pieces held, saved after 61 s"
        );
    }

    /// The quick pass writes a torrent only when its progress moved, and never
    /// a finished one: those wait for the full sweep.
    #[tokio::test(start_paused = true)]
    async fn the_quick_pass_skips_what_did_not_move_and_what_is_finished() {
        let (f, chunks) = four_piece_fixture("fastskip");
        start_session(&f, "").await;
        download(&f, 0, &chunks[0]).await;
        let_pass(61).await;
        assert_eq!(saved_bitfield_opt(&f).as_deref(), Some("80"));

        // Marked in the record: a rewrite would put the real value back.
        rusqlite::Connection::open(f.root.join("state.db"))
            .unwrap()
            .execute_batch("UPDATE torrent_state SET bitfield='zz'")
            .unwrap();
        let_pass(60).await;
        assert_eq!(
            saved_bitfield_opt(&f).as_deref(),
            Some("zz"),
            "no progress: not rewritten"
        );

        download(&f, 1, &chunks[1]).await;
        let_pass(60).await;
        assert_eq!(
            saved_bitfield_opt(&f).as_deref(),
            Some("c0"),
            "progress: rewritten"
        );

        // Whole now: the quick pass leaves it to the full sweep.
        rusqlite::Connection::open(f.root.join("state.db"))
            .unwrap()
            .execute_batch("UPDATE torrent_state SET bitfield='zz'")
            .unwrap();
        download(&f, 2, &chunks[2]).await;
        download(&f, 3, &chunks[3]).await;
        let_pass(60).await;
        assert_eq!(
            saved_bitfield_opt(&f).as_deref(),
            Some("zz"),
            "finished: left to the full sweep"
        );
    }

    /// A seeding torrent is never in the quick pass.
    #[tokio::test(start_paused = true)]
    async fn the_quick_pass_never_writes_a_seeding_torrent() {
        let (f, chunks) = four_piece_fixture("fastseed");
        for (i, c) in chunks.iter().enumerate() {
            download(&f, i as u32, c).await;
        }
        f.torrent.status.store(
            crate::torrent::meta::TorrentStatus::Seeding as u8,
            Ordering::Relaxed,
        );
        start_session(&f, "").await;
        let_pass(120).await;
        assert_eq!(
            saved_bitfield_opt(&f).as_deref(),
            Some(""),
            "only the record made at add, before the full sweep"
        );
    }

    /// 0 turns the quick pass off: back to one sweep every five minutes.
    #[tokio::test(start_paused = true)]
    async fn a_quick_pass_of_zero_seconds_is_off() {
        let (f, chunks) = four_piece_fixture("fastoff");
        start_session(&f, r#","resume_fast_secs":0"#).await;
        download(&f, 0, &chunks[0]).await;
        let_pass(250).await;
        assert_eq!(
            saved_bitfield_opt(&f).as_deref(),
            Some(""),
            "only the record made at add, before the five minutes"
        );
        let_pass(60).await;
        assert_eq!(
            saved_bitfield_opt(&f).as_deref(),
            Some("80"),
            "the full sweep still runs"
        );
    }

    /// A second engine over the first one's state, as after a restart, once
    /// `sql` has been run on the state database behind its back.
    fn restart_after(
        f: &Fixture,
        sql: &str,
    ) -> (
        Arc<crate::torrent::TorrentManager>,
        Arc<crate::torrent::meta::TorrentState>,
    ) {
        f._mgr.save_all_resume();
        rusqlite::Connection::open(f.root.join("state.db"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let mgr = Arc::new(crate::torrent::TorrentManager::new(
            f.root.join("data").to_string_lossy().into_owned(),
            f.root.join("resume").to_string_lossy().into_owned(),
            Arc::new(DiskManager::new(16)),
        ));
        let blob = f.torrent.metainfo_bytes().expect("the fixture's blob");
        mgr.set_blob_source(Arc::new(move |_: &str| Some(blob.clone())));
        assert_eq!(mgr.load_resume_data(), 1);
        let t = mgr.get(&f.torrent.info_hash).expect("restored");
        (mgr, t)
    }

    fn held_of(t: &crate::torrent::meta::TorrentState) -> Vec<u32> {
        match t.picker.get() {
            Some(p) => {
                let p = p.lock().unwrap();
                (0..4).filter(|i| p.has_piece(*i)).collect()
            }
            None => vec![0, 1, 2, 3],
        }
    }

    /// The torrent, once it holds exactly `want` or a second and a half have
    /// gone by (a recheck runs in the background).
    async fn held_after_settling(t: &crate::torrent::meta::TorrentState, want: &[u32]) -> Vec<u32> {
        for _ in 0..75 {
            if held_of(t) == want {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        held_of(t)
    }

    /// ⭐ A bitfield that cannot be read as hex is not a torrent at 0% that
    /// downloads again what is already on disk: the disk is asked, and the
    /// pieces it holds are recognised.
    #[tokio::test]
    async fn a_bitfield_that_is_not_hex_is_rechecked_against_the_disk() {
        let (f, chunks) = four_piece_fixture("nothex");
        download(&f, 0, &chunks[0]).await;
        download(&f, 1, &chunks[1]).await;
        let (mgr, t) = restart_after(&f, "UPDATE torrent_state SET bitfield='zz'");

        assert_eq!(
            held_after_settling(&t, &[0, 1]).await,
            vec![0, 1],
            "what is on disk is recognised"
        );
        let (_, issues) = mgr.startup_issues();
        assert!(
            issues
                .iter()
                .any(|i| i.info_hash == hex_of(&f) && i.kind == "bitfield_discarded"),
            "and the anomaly is listed: {issues:?}"
        );
    }

    /// The same for a bitfield that no longer matches the sum stored with it:
    /// the torrent is loaded, the claim set aside, the disk asked.
    #[tokio::test]
    async fn a_bitfield_that_fails_its_checksum_is_rechecked_against_the_disk() {
        let (f, chunks) = four_piece_fixture("badsum");
        download(&f, 0, &chunks[0]).await;
        download(&f, 1, &chunks[1]).await;
        let (mgr, t) = restart_after(
            &f,
            "DROP TRIGGER IF EXISTS bitfield_sum_reset; UPDATE torrent_state SET bitfield='f0'",
        );

        assert_eq!(
            held_after_settling(&t, &[0, 1]).await,
            vec![0, 1],
            "only what the disk holds"
        );
        let (_, issues) = mgr.startup_issues();
        assert!(
            issues.iter().any(|i| i.kind == "bitfield_discarded"),
            "{issues:?}"
        );
    }

    /// A torrent stopped before the restart, its bitfield set aside: the
    /// check is asked at load, and the stop put back just after it
    /// (`restore_stopped`, as at startup). That is no stop asked now: the
    /// check still runs, its verdict lands, and the torrent stays stopped.
    #[tokio::test]
    async fn a_stopped_torrent_with_an_unusable_bitfield_is_checked_and_stays_stopped() {
        use std::sync::atomic::Ordering;
        let (f, chunks) = four_piece_fixture("nothexstopped");
        download(&f, 0, &chunks[0]).await;
        download(&f, 1, &chunks[1]).await;
        let (mgr, t) = restart_after(&f, "UPDATE torrent_state SET bitfield='zz'");
        assert!(
            t.rechecking.load(Ordering::SeqCst),
            "the check is asked at load"
        );
        mgr.restore_stopped(&f.torrent.info_hash).expect("restored");

        assert_eq!(
            held_after_settling(&t, &[0, 1]).await,
            vec![0, 1],
            "the check ran to its verdict"
        );
        for _ in 0..150 {
            if !t.rechecking.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!t.rechecking.load(Ordering::SeqCst));
        assert!(t.is_paused.load(Ordering::Relaxed), "still stopped");
        assert_eq!(
            t.status.load(Ordering::Relaxed),
            crate::torrent::meta::TorrentStatus::Stopped as u8,
            "not started by the verdict"
        );
    }

    fn hex_of(f: &Fixture) -> String {
        crate::torrent::hex_encode(&f.torrent.info_hash)
    }

    #[tokio::test]
    async fn the_zero_copy_path_declines_a_request_it_cannot_satisfy_whole() {
        let content = vec![2u8; 1024];
        let f = fixture("sendfile-decline", &content);
        assert!(f
            .disk
            .write_piece(&f.torrent, 0, content.clone())
            .await
            .unwrap());
        assert!(
            f.disk.block_file(&f.torrent, 0, 0, 99_999).is_none(),
            "a length the file cannot satisfy is declined, not truncated"
        );
    }
}

#[cfg(test)]
mod claim_probe_tests {
    use super::*;

    fn sha1_of(data: &[u8]) -> [u8; 20] {
        use sha1::{Digest, Sha1};
        Sha1::digest(data).into()
    }

    /// A file named by another torrent -- one link, nothing else gives it away
    /// -- is compared too, once the host says which files those are.
    #[tokio::test]
    async fn a_file_another_torrent_names_is_compared() {
        use crate::torrent::TorrentManager;
        let content: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let root = std::env::temp_dir().join(format!("typhon-claim-probe-{}", std::process::id()));
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(root.join("resume")).unwrap();
        let disk = Arc::new(DiskManager::new(16));
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            root.join("resume").to_string_lossy().into_owned(),
            disk.clone(),
        ));
        let mut info =
            b"d6:lengthi4096e4:name11:payload.bin12:piece lengthi4096e6:pieces20:".to_vec();
        info.extend_from_slice(&sha1_of(&content));
        info.push(b'e');
        let mut bytes = b"d4:info".to_vec();
        bytes.extend_from_slice(&info);
        bytes.push(b'e');
        let blob = bytes.clone();
        mgr.set_blob_source(Arc::new(move |_: &str| Some(blob.clone())));
        let save = data.to_string_lossy().into_owned();
        let (ih, _) = mgr.add_torrent_bytes(&bytes, &save, true, false).unwrap();
        let t = mgr.get(&ih).unwrap();
        disk.set_claim_probe(Arc::new(|_| {
            [PathBuf::from("payload.bin")].into_iter().collect()
        }));
        let mut other = content.clone();
        other[10] ^= 1;
        std::fs::write(data.join("payload.bin"), &other).unwrap();
        let why = disk.write_piece(&t, 0, content.clone()).await.unwrap_err();
        assert!(why.starts_with("refused to overwrite"), "{why}");
        assert_eq!(std::fs::read(data.join("payload.bin")).unwrap(), other);

        // The same bytes: the piece counts and the file is not written.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::write(data.join("payload.bin"), &content).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(data.join("payload.bin"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(disk.write_piece(&t, 0, content.clone()).await.unwrap());
        assert_eq!(
            std::fs::metadata(data.join("payload.bin"))
                .unwrap()
                .modified()
                .unwrap(),
            old,
            "written although identical"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
