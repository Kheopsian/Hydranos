//! BEP 19 webseed — "GetRight" style HTTP seeding from the `url-list` key.
//!
//! Some publishers ship torrents that have **no BitTorrent seeder at all**:
//! the tracker only exists so downloaders find each other, and the bytes come
//! from an HTTP mirror named in `url-list`. Internet Archive is the canonical
//! case — every one of its ~88M items carries a `url-list` pointing at
//! `archive.org/download/<item>/`, and none of them has a seed. Without this
//! module such a torrent sits at 0% for ever, with no error to explain it.
//!
//! Shape: a small fixed pool of workers, NOT one task per torrent. A catalogue
//! of a million torrents cannot afford a task each, and the useful parallelism
//! is bounded by the HTTP origin anyway (measured against archive.org: 2.4 MB/s
//! at one stream, 51 MB/s at 32 — still climbing, so the cap is a courtesy as
//! much as a limit).
//!
//! Each worker claims a torrent, drives it as far as it can, and releases it.
//! Claims live in `CLAIMED` so two workers never fight over the same picker.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::{DashMap, DashSet};
use tracing::{info, warn};

use crate::config::EngineConfig;
use crate::disk::DiskManager;
use crate::torrent::meta::{FileEntry, InfoHash, TorrentMeta, TorrentState, TorrentStatus};
use crate::torrent::TorrentManager;

/// Same block size the peer path uses. Pieces are handed to the picker in
/// these units so webseed data goes through the exact bounds/alignment checks
/// that guard `receive_block` — a short or overlong HTTP body must not be able
/// to complete a piece with a hole in it.
const BLOCK_SIZE: u32 = 16384;

/// How many pieces one worker pulls before releasing a torrent back to the
/// pool. Most webseed torrents are small (Internet Archive median: 2.2 MB,
/// about 5 pieces), so this finishes the typical torrent in a single claim
/// while still letting a 20 GB item share the pool.
const MAX_PIECES_PER_CLAIM: u32 = 64;

/// Consecutive failures before a torrent is parked. A deleted or renamed item
/// answers 404 for ever; at catalogue scale that must not become a permanent
/// retry storm against the origin.
const MAX_FAILS: u32 = 5;
const PARK_SECS: u64 = 3600;

static CLAIMED: std::sync::OnceLock<DashSet<InfoHash>> = std::sync::OnceLock::new();
/// info_hash -> (consecutive failures, unix time before which not to retry)
static BACKOFF: std::sync::OnceLock<DashMap<InfoHash, (u32, u64)>> = std::sync::OnceLock::new();

fn claimed() -> &'static DashSet<InfoHash> {
    CLAIMED.get_or_init(DashSet::new)
}

fn backoff() -> &'static DashMap<InfoHash, (u32, u64)> {
    BACKOFF.get_or_init(DashMap::new)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Percent-encode one path segment. `url-list` values are directory prefixes
/// and the rest of the URL is built from torrent-supplied names, which routinely
/// contain spaces, accents and `#`. Encoding per segment (not over the whole
/// string) keeps the `/` separators intact.
fn enc_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// BEP 19 URL for one file of the torrent.
///
/// Multi-file: the `url-list` entry is a directory, and the file sits at
/// `<base>/<name>/<path...>`. Single-file: a base ending in `/` is a directory
/// holding `<name>`; anything else IS the file itself.
fn file_url(base: &str, meta: &TorrentMeta, f: &FileEntry) -> String {
    let trimmed = base.trim_end_matches('/');
    if meta.multi_file {
        let mut u = format!("{}/{}", trimmed, enc_segment(&meta.name));
        for c in f.path.components() {
            u.push('/');
            u.push_str(&enc_segment(&c.as_os_str().to_string_lossy()));
        }
        u
    } else if base.ends_with('/') {
        format!("{}/{}", trimmed, enc_segment(&meta.name))
    } else {
        base.to_string()
    }
}

/// GET one byte range. `to` is inclusive, as in the HTTP header.
///
/// A server that ignores `Range` answers 200 with the whole file. Accepting
/// that would pull a multi-gigabyte body to satisfy a 512 KB piece, so it is
/// only tolerated when the range asked for happens to be the entire file.
async fn fetch_range(
    client: &reqwest::Client,
    url: &str,
    from: u64,
    to: u64,
    file_len: u64,
) -> Result<Vec<u8>, String> {
    let want = (to - from + 1) as usize;
    let resp = client
        .get(url)
        .header("Range", format!("bytes={}-{}", from, to))
        .send()
        .await
        .map_err(|e| format!("GET {}: {}", url, e))?;

    let status = resp.status();
    let whole_file = from == 0 && to + 1 >= file_len;
    if status.as_u16() != 206 && !(status.is_success() && whole_file) {
        return Err(format!("{} answered {} for bytes={}-{}", url, status, from, to));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("body {}: {}", url, e))?;
    if body.len() != want {
        return Err(format!(
            "{} returned {} bytes for a {}-byte range",
            url,
            body.len(),
            want
        ));
    }
    Ok(body.to_vec())
}

/// Assemble one whole piece from the HTTP mirror.
///
/// A piece is a window over the concatenated file stream, so it can straddle
/// several files; each overlapping file contributes one ranged GET.
async fn fetch_piece(
    client: &reqwest::Client,
    meta: &TorrentMeta,
    base: &str,
    index: u32,
) -> Result<Vec<u8>, String> {
    let piece_size = meta.piece_size(index) as u64;
    let piece_start = index as u64 * meta.piece_length as u64;
    let piece_end = piece_start + piece_size; // exclusive

    let mut out: Vec<u8> = Vec::with_capacity(piece_size as usize);
    for f in &meta.files {
        if f.length == 0 {
            continue; // a zero-length file occupies no range: skip, never GET
        }
        let f_end = f.offset + f.length;
        if f.offset >= piece_end || f_end <= piece_start {
            continue;
        }
        let from = piece_start.max(f.offset) - f.offset;
        let to = piece_end.min(f_end) - f.offset - 1; // inclusive
        let url = file_url(base, meta, f);
        let chunk = fetch_range(client, &url, from, to, f.length).await?;
        out.extend_from_slice(&chunk);
    }

    if out.len() as u64 != piece_size {
        return Err(format!(
            "piece {} assembled to {} bytes, expected {}",
            index,
            out.len(),
            piece_size
        ));
    }
    Ok(out)
}

/// True when this torrent still wants webseed help right now.
fn wants_webseed(t: &TorrentState) -> bool {
    if t.meta.url_list.is_empty() || t.seed_mode {
        return false;
    }
    if t.is_removed.load(Ordering::Relaxed) || t.is_paused.load(Ordering::Relaxed) {
        return false;
    }
    if t.status.load(Ordering::Relaxed) != TorrentStatus::Downloading as u8 {
        return false;
    }
    match t.picker.get() {
        Some(p) => !p.lock().unwrap().is_complete(),
        None => false,
    }
}

/// Pull pieces for one torrent until it completes, stalls, or hits the claim
/// budget. Returns how many pieces were verified and written.
async fn drive_torrent(
    client: &reqwest::Client,
    t: &Arc<TorrentState>,
    disk: &Arc<DiskManager>,
) -> Result<u32, String> {
    // A webseed has everything, so the picker is asked with an all-ones
    // bitfield. Availability is deliberately NOT incremented: the swarm
    // availability figure describes peers, and an HTTP mirror is not one.
    let num_pieces = t.meta.num_pieces() as usize;
    let have_all = vec![0xFFu8; num_pieces.div_ceil(8)];
    let mut done = 0u32;

    for _ in 0..MAX_PIECES_PER_CLAIM {
        if !wants_webseed(t) {
            break;
        }
        let picker = match t.picker.get() {
            Some(p) => p,
            None => break,
        };

        // Reserve a piece. The lock is released before any await: holding a
        // std Mutex across .await would poison the whole download path.
        let index = {
            let mut p = picker.lock().unwrap();
            match p.pick_piece(&have_all) {
                Some(i) => {
                    let size = t.meta.piece_size(i);
                    p.start_piece(i, size, BLOCK_SIZE);
                    i
                }
                None => break,
            }
        };

        // Rotate over the mirrors so a multi-host url-list spreads load, and
        // fall through to the next one when a host is down.
        let mut data = None;
        let mut last_err = String::new();
        for k in 0..t.meta.url_list.len() {
            let base = &t.meta.url_list[(index as usize + k) % t.meta.url_list.len()];
            match fetch_piece(client, &t.meta, base, index).await {
                Ok(d) => {
                    data = Some(d);
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        let data = match data {
            Some(d) => d,
            None => {
                picker.lock().unwrap().cancel_piece(index);
                return Err(last_err);
            }
        };

        // Hand the bytes over block by block so they pass the same alignment
        // and length checks as anything arriving from a peer.
        let complete = {
            let mut p = picker.lock().unwrap();
            let mut c = false;
            let mut off = 0u32;
            while (off as usize) < data.len() {
                let end = (off as usize + BLOCK_SIZE as usize).min(data.len());
                c = p.receive_block(index, off, &data[off as usize..end]);
                off = end as u32;
            }
            c
        };
        if !complete {
            picker.lock().unwrap().cancel_piece(index);
            return Err(format!("piece {} refused by the picker", index));
        }

        let piece_data = { picker.lock().unwrap().take_piece_data(index) };
        let piece_data = match piece_data {
            Some(d) => d,
            None => continue,
        };
        if crate::peer::download::commit_piece(t, disk, index, piece_data).await {
            done += 1;
        } else {
            // SHA1 mismatch or write error: commit_piece already released the
            // piece. A mirror serving wrong bytes is a real failure, not a
            // retry-for-ever situation.
            return Err(format!("piece {} failed verification", index));
        }
    }
    Ok(done)
}

/// One worker: find an unclaimed torrent that wants webseed help, drive it,
/// release it.
async fn worker(mgr: Arc<TorrentManager>, disk: Arc<DiskManager>, client: reqwest::Client) {
    loop {
        let candidate = next_candidate(&mgr);
        let t = match candidate {
            Some(t) => t,
            None => {
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let ih = t.info_hash;
        let res = drive_torrent(&client, &t, &disk).await;
        claimed().remove(&ih);

        match res {
            Ok(_) => {
                backoff().remove(&ih);
            }
            Err(e) => {
                let mut entry = backoff().entry(ih).or_insert((0, 0));
                entry.0 += 1;
                if entry.0 >= MAX_FAILS {
                    entry.1 = now_secs() + PARK_SECS;
                    entry.0 = 0;
                    warn!(
                        "[webseed] {} parked for {}s after {} failures: {}",
                        crate::torrent::hex_encode(&ih)[..8].to_string(),
                        PARK_SECS,
                        MAX_FAILS,
                        e
                    );
                } else {
                    tracing::debug!(
                        "[webseed] {} failed: {}",
                        crate::torrent::hex_encode(&ih)[..8].to_string(),
                        e
                    );
                }
            }
        }
    }
}

/// Scan for the next torrent to work on. Skips anything already claimed by
/// another worker or parked by the failure backoff.
fn next_candidate(mgr: &TorrentManager) -> Option<Arc<TorrentState>> {
    let now = now_secs();
    // find_torrent walks the shard map in place: at a million torrents this
    // must not allocate a vector of every Arc just to pick one.
    mgr.find_torrent(|t| {
        let ih = t.info_hash;
        if let Some(b) = backoff().get(&ih) {
            if b.1 > now {
                return false;
            }
        }
        if !wants_webseed(t) {
            return false;
        }
        // insert() returns false when another worker already holds the claim.
        claimed().insert(ih)
    })
}

/// Build the HTTP client used for every webseed fetch.
///
/// It deliberately reuses `TYPHON_ANNOUNCE_PROXY`: a webseed GET is an outbound
/// request carrying our IP to a third party, exactly like an announce, so it
/// must leave by the same door. Anything else would re-open the leak the
/// announce proxy exists to close.
fn build_client(cfg: &EngineConfig) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .user_agent(cfg.user_agent.clone())
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(20))
        .pool_idle_timeout(Duration::from_secs(90));
    if let Ok(url) = std::env::var("TYPHON_ANNOUNCE_PROXY") {
        if !url.is_empty() {
            let p = reqwest::Proxy::all(&url).map_err(|e| format!("proxy {}: {}", url, e))?;
            b = b.proxy(p);
            info!("[webseed] fetches proxied via {}", url);
        }
    }
    b.build().map_err(|e| e.to_string())
}

/// Start the webseed pool. Does nothing when disabled, and refuses to run
/// rather than leak when the engine is pinned to a device it cannot honour.
pub fn start(mgr: Arc<TorrentManager>, cfg: &EngineConfig) {
    if !cfg.enable_webseed {
        info!("[webseed] disabled by config: url-list is parsed but never fetched");
        return;
    }
    let proxied = std::env::var("TYPHON_ANNOUNCE_PROXY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !cfg.bind_device.is_empty() && !proxied {
        // SO_BINDTODEVICE is applied to the sockets this engine opens itself;
        // reqwest opens its own, so a device-pinned engine with no proxy would
        // fetch straight out of the default route and publish the host IP to
        // the mirror. Refusing is the only safe answer.
        warn!(
            "[webseed] DISABLED: engine is pinned to '{}' but TYPHON_ANNOUNCE_PROXY is unset — \
             an HTTP fetch would bypass the pin and expose the host address",
            cfg.bind_device
        );
        return;
    }
    let client = match build_client(cfg) {
        Ok(c) => c,
        Err(e) => {
            warn!("[webseed] DISABLED: cannot build HTTP client: {}", e);
            return;
        }
    };
    let workers = cfg.webseed_max_concurrent.max(1);
    let disk = mgr.disk().clone();
    info!("[webseed] BEP 19 enabled, {} concurrent fetches", workers);
    for _ in 0..workers {
        let mgr = mgr.clone();
        let disk = disk.clone();
        let client = client.clone();
        tokio::spawn(async move { worker(mgr, disk, client).await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn meta(multi: bool, name: &str, files: Vec<(&str, u64)>) -> TorrentMeta {
        let mut offset = 0u64;
        let files = files
            .into_iter()
            .map(|(p, len)| {
                let f = FileEntry {
                    path: PathBuf::from(p),
                    offset,
                    length: len,
                };
                offset += len;
                f
            })
            .collect();
        TorrentMeta {
            info_hash: [0u8; 20],
            name: name.to_string(),
            num_pieces: 1,
            piece_length: 16384,
            total_size: offset,
            files,
            trackers: vec![],
            url_list: vec![],
            private: false,
            multi_file: multi,
            info_dict_len: 0,
        }
    }

    /// The Internet Archive shape: a directory base, a multi-file torrent whose
    /// name is the item identifier.
    #[test]
    fn multi_file_url_is_base_name_path() {
        let m = meta(true, "my-item", vec![("a/b.txt", 10)]);
        assert_eq!(
            file_url("https://archive.org/download/", &m, &m.files[0]),
            "https://archive.org/download/my-item/a/b.txt"
        );
    }

    /// A base with no trailing slash still separates cleanly.
    #[test]
    fn multi_file_url_tolerates_missing_slash() {
        let m = meta(true, "item", vec![("f.bin", 4)]);
        assert_eq!(
            file_url("http://h/items", &m, &m.files[0]),
            "http://h/items/item/f.bin"
        );
    }

    /// Single-file, base is a directory -> append the torrent name.
    #[test]
    fn single_file_directory_base_appends_name() {
        let m = meta(false, "movie.mkv", vec![("movie.mkv", 100)]);
        assert_eq!(
            file_url("http://h/d/", &m, &m.files[0]),
            "http://h/d/movie.mkv"
        );
    }

    /// Single-file, base is the file itself -> use it verbatim. Appending the
    /// name here would produce `.../movie.mkv/movie.mkv` and 404 every fetch.
    #[test]
    fn single_file_direct_base_is_used_as_is() {
        let m = meta(false, "movie.mkv", vec![("movie.mkv", 100)]);
        assert_eq!(
            file_url("http://h/d/movie.mkv", &m, &m.files[0]),
            "http://h/d/movie.mkv"
        );
    }

    /// Names routinely carry spaces and accents; each segment is encoded on its
    /// own so the separators survive.
    #[test]
    fn segments_are_percent_encoded_but_slashes_survive() {
        let m = meta(true, "a b", vec![("dir/é f.txt", 3)]);
        assert_eq!(
            file_url("http://h/", &m, &m.files[0]),
            "http://h/a%20b/dir/%C3%A9%20f.txt"
        );
    }
}
