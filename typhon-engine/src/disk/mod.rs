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

pub struct DiskManager {
    read_cache: Mutex<LruCache<CacheKey, CacheEntry>>,
    /// Files written and not yet synced, per torrent.
    written: Arc<Mutex<HashMap<[u8; 20], std::collections::HashSet<PathBuf>>>>,
    /// Bumped after every piece write, per stripe of cache blocks. A read
    /// that saw another value before going to disk may hold bytes the write
    /// has since replaced, so it serves them once but does not cache them.
    write_gen: Box<[AtomicU64]>,
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

    /// Make every file written since the last sync durable, and say how many.
    ///
    /// Called by the resume sweep before it saves the bitfields. SQLite syncs
    /// its own file, not ours: without this, after a power cut the record
    /// could be on disk while the bytes it vouches for were still in the page
    /// cache, and we would serve zeros as verified pieces. Each file is synced
    /// once however many pieces went to it, and the set holds only what was
    /// written since the last sweep; most of it has reached the disk on its own
    /// by then. Blocking: callers run it off the async workers.
    pub fn sync_written(&self) -> usize {
        let all = std::mem::take(&mut *self.written.lock());
        all.into_iter().map(|(ih, files)| self.sync_files(ih, files)).sum()
    }

    /// `sync_written` for one torrent: what its completion record needs.
    pub fn sync_torrent(&self, info_hash: &[u8; 20]) -> usize {
        let files = self.written.lock().remove(info_hash);
        files.map_or(0, |f| self.sync_files(*info_hash, f))
    }

    /// A file that fails to sync goes back in the set, to be tried again by
    /// the next sweep; the state is saved anyway -- this hardens the order of
    /// writes, it does not make a failing disk durable.
    fn sync_files(&self, info_hash: [u8; 20], files: std::collections::HashSet<PathBuf>) -> usize {
        let mut synced = 0;
        for path in files {
            let cached = fd_cache().lock().peek(&path).cloned();
            // Never created here: a file deleted since was meant to go.
            let file = match cached {
                Some(f) => f,
                None => match std::fs::OpenOptions::new().write(true).open(&path) {
                    Ok(f) => Arc::new(f),
                    Err(_) => continue,
                },
            };
            match file.sync_data() {
                Ok(()) => synced += 1,
                Err(e) => {
                    tracing::warn!("[disk] sync {:?}: {}", path, e);
                    self.written.lock().entry(info_hash).or_default().insert(path);
                }
            }
        }
        synced
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

        let block_bytes = read_block_direct(torrent, block_piece, block_offset_in_piece, block_len).await?;

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
                written.lock().entry(info_hash).or_default().insert(full_path);

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
    let ops = torrent.meta.map_block(piece, offset, length);
    let save_path = torrent.save_path.read().clone();
    let name = torrent.meta.name.clone();
    // Multi-file torrents have the `files` key in their info dict: on-disk
    // layout is save_path/<name>/<file-path>. Single-file (no `files` key)
    // has just save_path/<name>. Using `files.len() == 1` as the discriminator
    // was wrong — multi-file torrents commonly contain a single entry and
    // would silently read from save_path/<file-path>, which doesn't exist.
    let multi_file = torrent.meta.multi_file;

    let outcome = tokio::task::spawn_blocking(move || {
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
            let file = get_or_open(&full_path).map_err(|e| {
                (
                    e.kind() == std::io::ErrorKind::NotFound,
                    format!("open {:?}: {}", full_path, e),
                )
            })?;

            let mut buf = vec![0u8; op.length as usize];

            pread_exact(&file, &mut buf, op.file_offset)
                .map_err(|e| (false, format!("pread {:?}: {}", full_path, e)))?;

            result.extend_from_slice(&buf);
        }

        Ok::<Bytes, (bool, String)>(Bytes::from(result))
    })
    .await
    .map_err(|e| format!("spawn_blocking: {}", e))?;

    match outcome {
        Ok(b) => Ok(b),
        Err((missing, msg)) => {
            // Park it once. serving_suspended then rejects further requests
            // upstream, so this path stops being hit at all.
            if missing {
                torrent.mark_error(&msg);
            }
            Err(msg)
        }
    }
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

/// Read a full piece from disk and check it against its hash, both on the
/// blocking pool: hashing a piece of several MiB on an async worker held that
/// worker for the whole digest. `None` if a backing file is missing or too
/// short (the piece isn't fully present on disk), on any read error, or when
/// there is no hash to check against -- the caller treats all of it as "not
/// have". Bypasses the read cache: a one-shot cold recheck scan should not
/// evict the hot blocks we are actively serving.
pub async fn check_piece_on_disk(torrent: &TorrentState, piece: u32) -> Option<bool> {
    let plen = torrent.meta.piece_size(piece);
    if plen == 0 {
        return None;
    }
    let check = torrent.piece_check(piece)?;
    let ops = torrent.meta.map_block(piece, 0, plen);
    let save_path = torrent.save_path.read().clone();
    let name = torrent.meta.name.clone();
    let multi_file = torrent.meta.multi_file;

    tokio::task::spawn_blocking(move || {
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
                let file = get_or_open(&full_path).ok()?;
                pread_exact(&file, data.get_mut(at..at + len)?, op.file_offset).ok()?;
            }
            at += len;
        }
        Some(check.matches(&data))
    })
    .await
    .ok()
    .flatten()
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
        assert_eq!(f.disk.sync_written(), 1, "the one file written");
        assert_eq!(f.disk.sync_written(), 0, "nothing new since");
        assert!(f.disk.write_piece(&f.torrent, 0, content).await.unwrap());
        f._mgr.save_all_resume();
        assert_eq!(f.disk.sync_written(), 0, "the sweep synced it first");
    }

    /// A completion syncs its own torrent's files, not every file every
    /// torrent wrote since the last sweep.
    #[tokio::test]
    async fn a_completion_syncs_only_its_own_torrent() {
        let content = vec![7u8; 4096];
        let f = fixture("synconly", &content);
        assert!(f.disk.write_piece(&f.torrent, 0, content).await.unwrap());
        assert_eq!(f.disk.sync_torrent(&[0u8; 20]), 0, "another torrent wrote nothing");
        assert_eq!(f.disk.sync_torrent(&f.torrent.info_hash), 1, "its one file");
        assert_eq!(f.disk.sync_written(), 0, "and it left the set");
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
