use std::fs::File;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    let f = match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => Arc::new(f),
        // A file we may read but not write -- a read-only mount, a 0444 seed
        // -- is still served, from a read-only handle that is NOT cached: a
        // later write to it opens its own, and fails for the real reason.
        Err(e) if matches!(
            e.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
        ) => {
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
            .open(path)?,
    );
    let mut cache = fd_cache().lock();
    cache.put(path.clone(), f.clone());
    Ok(f)
}

use bytes::Bytes;
use lru::LruCache;
use std::num::NonZeroUsize;

use sha1::{Sha1, Digest};
use crate::torrent::meta::TorrentState;

const CACHE_BLOCK_SIZE: u64 = 131072;        // 128 KB
const CACHE_MAX_ENTRIES: usize = 2048;        // ~256 MB
const CACHE_TTL: Duration = Duration::from_secs(60);

pub static DISK_CACHE_HIT: AtomicU64 = AtomicU64::new(0);
pub static DISK_CACHE_MISS: AtomicU64 = AtomicU64::new(0);
pub static DISK_CACHE_STALE: AtomicU64 = AtomicU64::new(0);
pub static DISK_CACHE_BYPASS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
struct CacheKey {
    info_hash: [u8; 20],
    block_start: u64,
}

struct CacheEntry {
    data: Arc<Vec<u8>>,
    inserted_at: Instant,
}

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

pub struct DiskManager {
    read_cache: Mutex<LruCache<CacheKey, CacheEntry>>,
    /// Files written and not yet synced, per torrent, with the pieces that
    /// went to each: a file that then fails to sync says which pieces it
    /// takes down with it.
    written: Arc<Mutex<HashMap<[u8; 20], HashMap<PathBuf, Vec<u32>>>>>,
    /// Bumped after every piece write, per stripe of cache blocks. A read
    /// that saw another value before going to disk may hold bytes the write
    /// has since replaced, so it serves them once but does not cache them.
    write_gen: Box<[AtomicU64]>,
}

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

const WRITE_GEN_STRIPES: usize = 256;

fn gen_stripe(key: &CacheKey) -> usize {
    let mut h = [0u8; 8];
    h.copy_from_slice(&key.info_hash[..8]);
    (u64::from_le_bytes(h) ^ (key.block_start / CACHE_BLOCK_SIZE)) as usize % WRITE_GEN_STRIPES
}

impl DiskManager {
    /// `max_open_files` is not used: descriptors live in the process-wide
    /// cache (`fd_cache`), fixed at 10 000. The per-manager pool this sized
    /// was never read, and a setting that looks like it acts is worse than
    /// none.
    pub fn new(_max_open_files: usize) -> Self {
        Self {
            read_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(CACHE_MAX_ENTRIES).unwrap()
            )),
            written: Arc::new(Mutex::new(HashMap::new())),
            write_gen: (0..WRITE_GEN_STRIPES).map(|_| AtomicU64::new(0)).collect(),
        }
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

    /// Read a block from disk for a torrent piece request.
    /// 128KB LRU block cache, TTL 60s — 1 miss covers 7 subsequent 16KB block requests.
    pub async fn read_block(
        &self,
        torrent: &TorrentState,
        piece: u32,
        offset: u32,
        length: u32,
    ) -> Result<Bytes, String> {
        let piece_size = torrent.meta.piece_length as u64;
        let abs_start = piece as u64 * piece_size + offset as u64;
        let block_start = (abs_start / CACHE_BLOCK_SIZE) * CACHE_BLOCK_SIZE;
        let offset_in_block = (abs_start - block_start) as usize;

        // Bypass: request spans two cache blocks or exceeds block size.
        if (length as u64) > CACHE_BLOCK_SIZE
            || (offset_in_block + length as usize) > CACHE_BLOCK_SIZE as usize
        {
            DISK_CACHE_BYPASS.fetch_add(1, Ordering::Relaxed);
            return read_block_direct(torrent, piece, offset, length).await;
        }

        let key = CacheKey { info_hash: torrent.info_hash, block_start };

        {
            let mut cache = self.read_cache.lock();
            if let Some(entry) = cache.get(&key) {
                if entry.inserted_at.elapsed() < CACHE_TTL {
                    DISK_CACHE_HIT.fetch_add(1, Ordering::Relaxed);
                    let data = entry.data.clone();
                    drop(cache);
                    let end = offset_in_block + length as usize;
                    if end > data.len() {
                        return Err(format!("cached block too short: {}..{} vs {}",
                            offset_in_block, end, data.len()));
                    }
                    return Ok(Bytes::copy_from_slice(&data[offset_in_block..end]));
                } else {
                    DISK_CACHE_STALE.fetch_add(1, Ordering::Relaxed);
                    cache.pop(&key);
                }
            }
        }

        DISK_CACHE_MISS.fetch_add(1, Ordering::Relaxed);
        let stripe = gen_stripe(&key);
        let seen = self.write_gen[stripe].load(Ordering::SeqCst);

        let block_end = (block_start + CACHE_BLOCK_SIZE).min(torrent.meta.total_size);
        // A block past the end must not wrap the subtraction into a huge read.
        let block_len = match block_end.checked_sub(block_start) {
            Some(n) => n as u32,
            None => return Err(format!("block start {} past end {}", block_start, block_end)),
        };
        let block_piece = (block_start / piece_size) as u32;
        let block_offset_in_piece = (block_start % piece_size) as u32;

        let block_bytes = match read_range(torrent, block_piece, block_offset_in_piece, block_len).await {
            Ok(b) => b,
            // The 128 KB window around the request may reach into sectors the
            // request does not need: ask for the request alone before blaming
            // anything, and blame only what it spans.
            Err(first) if first.unreadable => {
                return match read_range(torrent, piece, offset, length).await {
                    Ok(b) => Ok(b),
                    Err(e) => Err(give_up_on(torrent, piece, offset, length, e)),
                };
            }
            Err(e) => return Err(give_up_on(torrent, piece, offset, length, e)),
        };

        let end = offset_in_block + length as usize;
        if end > block_bytes.len() {
            return Err(format!("short block read: need {}..{} got {}",
                offset_in_block, end, block_bytes.len()));
        }
        let result = Bytes::copy_from_slice(&block_bytes[offset_in_block..end]);

        let block_vec: Vec<u8> = block_bytes.to_vec();
        {
            let mut cache = self.read_cache.lock();
            // Checked under the cache lock, which the write takes after its
            // bump: either this put lands first and the write pops it, or the
            // write came first and this read may predate it.
            if self.write_gen[stripe].load(Ordering::SeqCst) == seen {
                cache.put(key, CacheEntry {
                    data: Arc::new(block_vec),
                    inserted_at: Instant::now(),
                });
            }
        }

        Ok(result)
    }

    /// Write a complete piece to disk and verify SHA1.
    pub async fn write_piece(
        &self,
        torrent: &TorrentState,
        piece: u32,
        data: Vec<u8>,
    ) -> Result<bool, String> {
        // Invalidate cached read blocks overlapping this piece
        let piece_size = torrent.meta.piece_length as u64;
        let piece_start = piece as u64 * piece_size;
        let piece_end = piece_start + data.len() as u64;
        let first_block = (piece_start / CACHE_BLOCK_SIZE) * CACHE_BLOCK_SIZE;
        self.invalidate(torrent.info_hash, first_block, piece_end);

        let ops = torrent.meta.map_block(piece, 0, data.len() as u32);
        let save_path = torrent.save_path.read().clone();
        let name = torrent.meta.name.clone();
        let multi_file = torrent.meta.multi_file;
        let written = self.written.clone();
        let info_hash = torrent.info_hash;

        // A piece we cannot check is a piece we refuse, not one we accept.
        let check = match torrent.piece_check(piece) {
            Some(c) => c,
            None => return Err(format!(
                "no piece hashes for {}; refusing to accept piece {}",
                torrent.info_hash_hex(), piece
            )),
        };

        let outcome = tokio::task::spawn_blocking(move || {
            if !check.matches(&data) {
                return Ok(false);
            }
            let mut data_offset = 0usize;
            for op in ops {
                // Alignment padding: zeros no file holds.
                if op.pad {
                    data_offset += op.length as usize;
                    continue;
                }
                let full_path = if multi_file {
                    save_path.join(&name).join(&op.path)
                } else {
                    save_path.join(&op.path)
                };

                // Said as what it is: swallowed, a folder that could not be
                // made (permissions, quota, a file in the way) came back as
                // an open error that no longer named the cause.
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("create folder {:?}: {}", parent, e))?;
                }

                let file = get_or_open_rw(&full_path)
                    .map_err(|e| format!("open write {:?}: {}", full_path, e))?;

                {
                    let chunk = &data[data_offset..data_offset + op.length as usize];
                    pwrite_all(&file, chunk, op.file_offset)
                        .map_err(|e| format!("pwrite {:?}: {}", full_path, e))?;
                }
                {
                    let mut w = written.lock();
                    let pieces = w.entry(info_hash).or_default().entry(full_path).or_default();
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
        // Again once the bytes are down: a read that went to disk while the
        // write was under way may have read the old bytes, and must not leave
        // them cached over the piece just written.
        for key in blocks(torrent.info_hash, first_block, piece_end) {
            self.write_gen[gen_stripe(&key)].fetch_add(1, Ordering::SeqCst);
        }
        self.invalidate(torrent.info_hash, first_block, piece_end);
        outcome
    }

    fn invalidate(&self, info_hash: [u8; 20], first_block: u64, end: u64) {
        let mut cache = self.read_cache.lock();
        for key in blocks(info_hash, first_block, end) {
            cache.pop(&key);
        }
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
        let name = torrent.meta.name.clone();
        let full_path = if torrent.meta.multi_file {
            save_path.join(&name).join(&op.path)
        } else {
            save_path.join(&op.path)
        };
        get_or_open(&full_path).ok().map(|f| (f, op.file_offset))
    }
}

/// The cache blocks from `first_block` up to `end`.
fn blocks(info_hash: [u8; 20], first_block: u64, end: u64) -> impl Iterator<Item = CacheKey> {
    (first_block..end)
        .step_by(CACHE_BLOCK_SIZE as usize)
        .map(move |block_start| CacheKey { info_hash, block_start })
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
pub fn path_is_zfs(_path: &std::path::Path) -> bool { false }

/// Non-blocking page-cache residency probe (preadv2 RWF_NOWAIT, 1 byte). True
/// if the block's first page is already cached -> safe to sendfile without
/// blocking the reactor. Cold -> caller uses the spawn_blocking buffered path.
#[cfg(unix)]
pub fn is_block_resident(file: &File, offset: u64) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut b = [0u8; 1];
    let iov = libc::iovec { iov_base: b.as_mut_ptr() as *mut libc::c_void, iov_len: 1 };
    // RWF_NOWAIT (0x8): returns EAGAIN instead of blocking if not in page cache.
    let n = unsafe { libc::preadv2(file.as_raw_fd(), &iov, 1, offset as libc::off_t, 0x8) };
    n == 1
}
#[cfg(not(unix))]
pub fn is_block_resident(_file: &File, _offset: u64) -> bool { false }

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
            if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
        }) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "send header 0")),
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
            if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
        }) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "sendfile 0")),
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
fn give_up_on(torrent: &TorrentState, piece: u32, offset: u32, length: u32, e: ReadFailure) -> String {
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
                pieces.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "),
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
    let name = torrent.meta.name.clone();
    // Multi-file torrents have the `files` key in their info dict: on-disk
    // layout is save_path/<name>/<file-path>. Single-file (no `files` key)
    // has just save_path/<name>. Using `files.len() == 1` as the discriminator
    // was wrong — multi-file torrents commonly contain a single entry and
    // would silently read from save_path/<file-path>, which doesn't exist.
    let multi_file = torrent.meta.multi_file;

    tokio::task::spawn_blocking(move || {
        let mut result = Vec::with_capacity(length as usize);

        for op in ops {
            if op.pad {
                result.resize(result.len() + op.length as usize, 0);
                continue;
            }
            let full_path = if multi_file {
                save_path.join(&name).join(&op.path)
            } else {
                save_path.join(&op.path)
            };

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
fn serve_read(file: &File, _path: &std::path::Path, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    pread_exact(file, buf, offset)
}

#[cfg(test)]
fn serve_read(file: &File, path: &std::path::Path, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    if read_failures().lock().contains(path) {
        return Err(std::io::Error::from_raw_os_error(5));
    }
    pread_exact(file, buf, offset)
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
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "seek_write returned 0"));
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
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "seek_read returned 0"));
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
    let name = torrent.meta.name.clone();
    let multi_file = torrent.meta.multi_file;

    let out = tokio::task::spawn_blocking(move || {
        let classify = classify_io;
        // Read straight into the piece: one buffer, no copy per file.
        let mut data = vec![0u8; plen as usize];
        let mut at = 0usize;
        for op in ops {
            let len = op.length as usize;
            if !op.pad {
                let full_path = if multi_file {
                    save_path.join(&name).join(&op.path)
                } else {
                    save_path.join(&op.path)
                };
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
        let blob = torrent_bytes_in_pieces("payload.bin", content, piece_len);
        mgr.set_blob_source(Arc::new(move |_hash: &str| Some(blob.clone())));

        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes_in_pieces("payload.bin", content, piece_len),
                data.to_string_lossy().as_ref(),
                true,
                false,
            )
            .expect("the fixture torrent parses");
        let torrent = mgr.get(&ih).expect("just added");
        Fixture { _mgr: mgr, torrent, disk, root }
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
        let ok = f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap();
        assert!(ok, "the piece hashes correctly and must be kept");

        let back = f.disk.read_block(&f.torrent, 0, 0, content.len() as u32).await.unwrap();
        assert_eq!(&back[..], &content[..], "what was written is what comes back");
    }

    /// ⭐ A read that starts past the end of the torrent is an error, not a
    /// subtraction that wraps into a read of gigabytes.
    #[tokio::test]
    async fn a_read_starting_past_the_end_is_an_error() {
        let content = vec![7u8; 4096];
        let f = fixture("pastend", &content);
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
        // 4096-byte torrent: offset 3 MiB is far beyond its only cache block.
        let r = f.disk.read_block(&f.torrent, 0, 3 << 20, 16).await;
        assert!(r.is_err(), "read past the end returned {:?}", r.map(|b| b.len()));
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
        let e = f.disk.write_piece(&f.torrent, 0, content).await.unwrap_err();
        assert!(e.contains("create folder"), "{e}");
    }

    /// ⭐ The files a piece went to are synced before the state saying we
    /// hold it is written, so a power cut cannot leave a piece marked held
    /// whose bytes never reached the disk. Each file once per sweep.
    #[tokio::test]
    async fn written_files_are_synced_before_the_state_is_saved() {
        let content = vec![7u8; 4096];
        let f = fixture("sync", &content);
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
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
        assert_eq!(f.disk.sync_torrent(&[0u8; 20]).synced, 0, "another torrent wrote nothing");
        assert_eq!(f.disk.sync_torrent(&f.torrent.info_hash).synced, 1, "its one file");
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
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());

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
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());

        let first = f.disk.read_block(&f.torrent, 0, 0, 512).await.unwrap();
        let second = f.disk.read_block(&f.torrent, 0, 0, 512).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(&first[..], &content[..512]);
    }

    /// ⭐ Writing a piece must INVALIDATE the blocks cached over it, or a
    /// re-checked torrent keeps serving the bytes it has just replaced.
    #[tokio::test]
    async fn rewriting_a_piece_invalidates_what_was_cached_over_it() {
        let content = vec![1u8; 4096];
        let f = fixture("invalidate", &content);
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
        let before = f.disk.read_block(&f.torrent, 0, 0, 16).await.unwrap();
        assert_eq!(&before[..], &[1u8; 16][..]);

        // The same bytes again: the write must still drop the cached block
        // rather than leave a stale one behind.
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
        let after = f.disk.read_block(&f.torrent, 0, 0, 16).await.unwrap();
        assert_eq!(&after[..], &before[..]);
    }

    /// A read past the end of the torrent is an error, never a short buffer a
    /// peer would take for real data.
    #[tokio::test]
    async fn a_read_past_the_end_is_an_error_not_a_short_read() {
        let content = vec![5u8; 1024];
        let f = fixture("past", &content);
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
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
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_racing_a_write_never_caches_what_the_write_replaced() {
        const PIECE: usize = 16 * 1024;
        let mut content = vec![1u8; PIECE];
        content.extend_from_slice(&[2u8; PIECE]);
        for round in 0..20 {
            let f = fixture_in_pieces(&format!("race{round}"), &content, PIECE);
            assert!(f.disk.write_piece(&f.torrent, 0, content[..PIECE].to_vec()).await.unwrap());
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
            assert_eq!(&got[..], &[2u8; 16][..], "round {round}: piece 1 as written");
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
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
        let path = f.root.join("data").join("payload.bin");
        assert!(holds_descriptor_for(&path), "the write cached its descriptor");

        f._mgr.remove_torrent(&f.torrent.info_hash, true).unwrap();
        assert!(path.exists(), "the data stays");
        assert!(!holds_descriptor_for(&path), "but no descriptor holds it any more");
    }

    /// Changing where a torrent lives drops the descriptors of where it was.
    #[tokio::test]
    async fn a_new_save_path_lets_go_of_the_old_files() {
        let content = vec![6u8; 4096];
        let f = fixture("relocate", &content);
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
        let old = f.root.join("data").join("payload.bin");
        assert!(holds_descriptor_for(&old));

        let moved = f.root.join("moved");
        std::fs::create_dir_all(&moved).unwrap();
        f._mgr
            .set_save_path(&f.torrent.info_hash, moved.to_string_lossy().as_ref())
            .unwrap();
        assert!(!holds_descriptor_for(&old), "the old path is no longer this torrent's");
    }

    fn four_piece_fixture(tag: &str) -> (Fixture, Vec<Vec<u8>>) {
        let mut content = Vec::new();
        for i in 0..4u8 {
            content.extend(std::iter::repeat(i + 1).take(4096));
        }
        let chunks = content.chunks(4096).map(|c| c.to_vec()).collect();
        let f = fixture_in_pieces(tag, &content, 4096);
        // The fixture is added stopped; these are downloads under way.
        f.torrent
            .status
            .store(crate::torrent::meta::TorrentStatus::Downloading as u8, Ordering::Relaxed);
        (f, chunks)
    }

    /// What `commit` does once a piece is down: written, then counted held.
    async fn download(f: &Fixture, piece: u32, data: &[u8]) {
        assert!(f.disk.write_piece(&f.torrent, piece, data.to_vec()).await.unwrap());
        f.torrent.picker.get().unwrap().lock().unwrap().set_have(piece);
    }

    fn held(f: &Fixture) -> Vec<u32> {
        let p = f.torrent.picker.get().unwrap().lock().unwrap();
        (0..4).filter(|i| p.has_piece(*i)).collect()
    }

    fn saved_bitfield(f: &Fixture) -> String {
        let db = crate::torrent::statedb::StateDb::open(&f.root.join("state.db")).unwrap();
        db.load_all().into_iter().next().expect("a record was saved").bitfield
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

        assert_eq!(held(&f), Vec::<u32>::new(), "nothing durable is vouched for");
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
        f.torrent
            .status
            .store(crate::torrent::meta::TorrentStatus::Seeding as u8, Ordering::Relaxed);
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
        f.torrent
            .status
            .store(crate::torrent::meta::TorrentStatus::Seeding as u8, Ordering::Relaxed);
        f._mgr.save_all_resume();
        fail_read_of(f.root.join("data").join("payload.bin"));

        let r = f.disk.read_block(&f.torrent, 1, 0, 16).await;
        assert!(r.is_err(), "the read fails");

        assert_eq!(held(&f), vec![0, 2, 3], "only the unreadable piece is given up");
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
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
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

        assert_eq!(saved_bitfield_opt(&f).as_deref(), Some("c0"), "two pieces held, saved after 61 s");
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
        assert_eq!(saved_bitfield_opt(&f).as_deref(), Some("zz"), "no progress: not rewritten");

        download(&f, 1, &chunks[1]).await;
        let_pass(60).await;
        assert_eq!(saved_bitfield_opt(&f).as_deref(), Some("c0"), "progress: rewritten");

        // Whole now: the quick pass leaves it to the full sweep.
        rusqlite::Connection::open(f.root.join("state.db"))
            .unwrap()
            .execute_batch("UPDATE torrent_state SET bitfield='zz'")
            .unwrap();
        download(&f, 2, &chunks[2]).await;
        download(&f, 3, &chunks[3]).await;
        let_pass(60).await;
        assert_eq!(saved_bitfield_opt(&f).as_deref(), Some("zz"), "finished: left to the full sweep");
    }

    /// A seeding torrent is never in the quick pass.
    #[tokio::test(start_paused = true)]
    async fn the_quick_pass_never_writes_a_seeding_torrent() {
        let (f, chunks) = four_piece_fixture("fastseed");
        for (i, c) in chunks.iter().enumerate() {
            download(&f, i as u32, c).await;
        }
        f.torrent
            .status
            .store(crate::torrent::meta::TorrentStatus::Seeding as u8, Ordering::Relaxed);
        start_session(&f, "").await;
        let_pass(120).await;
        assert_eq!(saved_bitfield_opt(&f).as_deref(), Some(""), "only the record made at add, before the full sweep");
    }

    /// 0 turns the quick pass off: back to one sweep every five minutes.
    #[tokio::test(start_paused = true)]
    async fn a_quick_pass_of_zero_seconds_is_off() {
        let (f, chunks) = four_piece_fixture("fastoff");
        start_session(&f, r#","resume_fast_secs":0"#).await;
        download(&f, 0, &chunks[0]).await;
        let_pass(250).await;
        assert_eq!(saved_bitfield_opt(&f).as_deref(), Some(""), "only the record made at add, before the five minutes");
        let_pass(60).await;
        assert_eq!(saved_bitfield_opt(&f).as_deref(), Some("80"), "the full sweep still runs");
    }

    /// A second engine over the first one's state, as after a restart, once
    /// `sql` has been run on the state database behind its back.
    fn restart_after(f: &Fixture, sql: &str) -> (Arc<crate::torrent::TorrentManager>, Arc<crate::torrent::meta::TorrentState>) {
        f._mgr.save_all_resume();
        rusqlite::Connection::open(f.root.join("state.db")).unwrap().execute_batch(sql).unwrap();
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

        assert_eq!(held_after_settling(&t, &[0, 1]).await, vec![0, 1], "what is on disk is recognised");
        let (_, issues) = mgr.startup_issues();
        assert!(
            issues.iter().any(|i| i.info_hash == hex_of(&f) && i.kind == "bitfield_discarded"),
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

        assert_eq!(held_after_settling(&t, &[0, 1]).await, vec![0, 1], "only what the disk holds");
        let (_, issues) = mgr.startup_issues();
        assert!(issues.iter().any(|i| i.kind == "bitfield_discarded"), "{issues:?}");
    }

    fn hex_of(f: &Fixture) -> String {
        crate::torrent::hex_encode(&f.torrent.info_hash)
    }

    #[tokio::test]
    async fn the_zero_copy_path_declines_a_request_it_cannot_satisfy_whole() {
        let content = vec![2u8; 1024];
        let f = fixture("sendfile-decline", &content);
        assert!(f.disk.write_piece(&f.torrent, 0, content.clone()).await.unwrap());
        assert!(
            f.disk.block_file(&f.torrent, 0, 0, 99_999).is_none(),
            "a length the file cannot satisfy is declined, not truncated"
        );
    }
}
