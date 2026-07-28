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

        let expected_hash = torrent.meta.pieces[piece as usize];
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

                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    let chunk = &data[data_offset..data_offset + op.length as usize];
                    file.write_at(chunk, op.file_offset)
                        .map_err(|e| format!("pwrite {:?}: {}", full_path, e))?;
                }

                data_offset += op.length as usize;
            }

            Ok(true)
        })
        .await
        .map_err(|e| format!("spawn_blocking: {}", e))?
    }
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

    tokio::task::spawn_blocking(move || {
        let mut result = Vec::with_capacity(length as usize);

        for op in ops {
            let full_path = if multi_file {
                save_path.join(&name).join(&op.path)
            } else {
                save_path.join(&op.path)
            };

            let file = get_or_open(&full_path)
                .map_err(|e| format!("open {:?}: {}", full_path, e))?;

            let mut buf = vec![0u8; op.length as usize];

            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                file.read_at(&mut buf, op.file_offset)
                    .map_err(|e| format!("pread {:?}: {}", full_path, e))?;
            }

            result.extend_from_slice(&buf);
        }

        Ok(Bytes::from(result))
    })
    .await
    .map_err(|e| format!("spawn_blocking: {}", e))?
}
