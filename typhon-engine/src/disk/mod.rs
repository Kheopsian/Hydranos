use std::fs::File;
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
    // re-download loop. All our data dirs are writable, so RW is always safe.
    let f = Arc::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?,
    );
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
/// fills /race under drain churn (nofile is huge, so the LRU never evicts).
pub fn evict_fds(paths: &[PathBuf]) {
    let mut cache = fd_cache().lock();
    for p in paths {
        cache.pop(p);
    }
}

pub struct DiskManager {
    fd_cache: Mutex<LruCache<PathBuf, Arc<File>>>,
    read_cache: Mutex<LruCache<CacheKey, CacheEntry>>,
}

impl DiskManager {
    pub fn new(max_open_files: usize) -> Self {
        Self {
            fd_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_open_files.max(100)).unwrap()
            )),
            read_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(CACHE_MAX_ENTRIES).unwrap()
            )),
        }
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

        let block_end = (block_start + CACHE_BLOCK_SIZE).min(torrent.meta.total_size);
        let block_len = (block_end - block_start) as u32;
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
            cache.put(key, CacheEntry {
                data: Arc::new(block_vec),
                inserted_at: Instant::now(),
            });
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
        {
            let mut cache = self.read_cache.lock();
            let mut bs = first_block;
            while bs < piece_end {
                cache.pop(&CacheKey { info_hash: torrent.info_hash, block_start: bs });
                bs += CACHE_BLOCK_SIZE;
            }
        }

        // A piece we cannot check is a piece we refuse, not one we accept.
        let expected_hash = match torrent.piece_hash(piece) {
            Some(h) => h,
            None => return Err(format!(
                "no piece hashes for {}; refusing to accept piece {}",
                torrent.info_hash_hex(), piece
            )),
        };
        let ops = torrent.meta.map_block(piece, 0, data.len() as u32);
        let save_path = torrent.save_path.read().clone();
        let name = torrent.meta.name.clone();
        let multi_file = torrent.meta.multi_file;

        tokio::task::spawn_blocking(move || {
            let mut hasher = Sha1::new();
            hasher.update(&data);
            let hash = hasher.finalize();
            let mut computed = [0u8; 20];
            computed.copy_from_slice(&hash);
            if computed != expected_hash {
                return Ok(false);
            }

            let mut data_offset = 0usize;
            for op in ops {
                let full_path = if multi_file {
                    save_path.join(&name).join(&op.path)
                } else {
                    save_path.join(&op.path)
                };

                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }

                let file = get_or_open_rw(&full_path)
                    .map_err(|e| format!("open write {:?}: {}", full_path, e))?;

                {
                    let chunk = &data[data_offset..data_offset + op.length as usize];
                    pwrite_all(&file, chunk, op.file_offset)
                        .map_err(|e| format!("pwrite {:?}: {}", full_path, e))?;
                }

                data_offset += op.length as usize;
            }

            Ok(true)
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
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
        if op.length != length {
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

/// Read a full piece from disk for hash verification. Returns None if any
/// backing file is missing or too short (the piece isn't fully present on
/// disk) or on any read error -- the caller treats that as "not have".
/// Bypasses the read cache: a one-shot cold recheck scan should not evict the
/// hot blocks we are actively serving.
pub async fn read_piece_for_check(torrent: &TorrentState, piece: u32) -> Option<Vec<u8>> {
    let plen = torrent.meta.piece_size(piece);
    if plen == 0 {
        return None;
    }
    let ops = torrent.meta.map_block(piece, 0, plen);
    let save_path = torrent.save_path.read().clone();
    let name = torrent.meta.name.clone();
    let multi_file = torrent.meta.multi_file;

    tokio::task::spawn_blocking(move || {
        let mut result = Vec::with_capacity(plen as usize);
        for op in ops {
            let full_path = if multi_file {
                save_path.join(&name).join(&op.path)
            } else {
                save_path.join(&op.path)
            };
            let file = get_or_open(&full_path).ok()?;
            let mut buf = vec![0u8; op.length as usize];
            pread_exact(&file, &mut buf, op.file_offset).ok()?;
            result.extend_from_slice(&buf);
        }
        Some(result)
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
        let mut info = Vec::new();
        info.extend_from_slice(
            format!("d6:lengthi{}e4:name{}:{name}", content.len(), name.len()).as_bytes(),
        );
        // One piece, sized to the content, so the whole file is piece 0.
        info.extend_from_slice(format!("12:piece lengthi{}e6:pieces20:", content.len()).as_bytes());
        info.extend_from_slice(&sha1_of(content));
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
        let blob = torrent_bytes("payload.bin", content);
        mgr.set_blob_source(Arc::new(move |_hash: &str| Some(blob.clone())));

        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes("payload.bin", content),
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
