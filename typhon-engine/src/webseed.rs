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

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::{DashMap, DashSet};
use futures::stream::StreamExt;
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

/// Bytes one HTTP round trip should aim to carry.
///
/// Latency against archive.org is ~2.5 s per request whatever its size, while a
/// single stream then carries 2.4-4 MB/s. A lone 512 KB piece therefore spends
/// roughly 80% of its life waiting for headers. 8 MB is the knee: transfer time
/// comes back to the same order as the latency, and 16 workers hold at most
/// 128 MB of piece buffers between them. Beyond it the curve flattens while the
/// memory keeps growing.
///
/// Most Internet Archive items are smaller than this (median 2.2 MB), so the
/// common case collapses to ONE request for the entire torrent.
const SPAN_TARGET_BYTES: u64 = 8 * 1024 * 1024;

/// Hard ceiling on pieces per span, for torrents whose piece_length is tiny
/// enough that 8 MB would mean thousands of them.
const SPAN_MAX_PIECES: usize = 64;

/// How many of a span's per-file requests may be in flight together.
///
/// BEP 19 gives one URL per file and a byte range cannot straddle two of
/// them, so a 2.2 MB Internet Archive item spread over its median 10 files
/// costs ten requests however the pieces are grouped. Measured in production:
/// issuing them in sequence gave 2.07 MB/s, matching ten round trips of
/// ~2.5 s per torrent almost exactly. Overlapping them turns those ten
/// latencies back into roughly one.
///
/// 6 keeps the engine-wide ceiling near 96 in-flight requests at the default
/// 16 workers — the neighbourhood of the 32-stream bench that reached
/// 51 MB/s without finding a limit, rather than a leap past anything
/// measured.
const SPAN_FILE_PARALLEL: usize = 6;

/// How many pieces one worker pulls before releasing a torrent back to the
/// pool. Most webseed torrents are small (Internet Archive median: 2.2 MB,
/// about 5 pieces), so this finishes the typical torrent in a single claim
/// while still letting a 20 GB item share the pool.
const MAX_PIECES_PER_CLAIM: u32 = 64;

/// Consecutive failures before a torrent is parked. A deleted or renamed item
/// answers 404 for ever; at catalogue scale that must not become a permanent
/// retry storm against the origin.
/// Refill the work queue once it drops below this.
const QUEUE_LOW: usize = 64;
/// How many candidates one walk of the catalogue gathers.
const QUEUE_TARGET: usize = 2048;

const MAX_FAILS: u32 = 5;
const PARK_SECS: u64 = 3600;

/// Wall-clock nanoseconds spent in each phase of the worker loop, summed
/// across every worker. Ratios between them are what matter: they say which
/// phase actually owns the throughput, which six rounds of reasoning about the
/// design did not manage to establish.
static T_WAIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static T_PICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static T_FETCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static T_FEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static T_COMMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static N_SPANS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static N_REQS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static N_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn add_ns(c: &std::sync::atomic::AtomicU64, t: std::time::Instant) {
    c.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

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

/// Byte window covered by pieces `first..=last`, as a half-open range.
///
/// Pulled out as a plain function so the span arithmetic is testable without a
/// torrent, an engine or a network.
fn span_byte_range(meta: &TorrentMeta, first: u32, last: u32) -> (u64, u64) {
    let start = first as u64 * meta.piece_length as u64;
    let end = last as u64 * meta.piece_length as u64 + meta.piece_size(last) as u64;
    (start, end)
}

/// The per-file ranged requests a span resolves to, in stream order.
///
/// Split out as a pure function: this is where an off-by-one silently corrupts
/// a piece, and it can be checked without a network.
fn span_file_ranges(
    meta: &TorrentMeta,
    base: &str,
    first: u32,
    last: u32,
) -> Vec<(String, u64, u64, u64)> {
    let (span_start, span_end) = span_byte_range(meta, first, last);
    let mut reqs = Vec::new();
    for f in &meta.files {
        if f.length == 0 {
            continue; // a zero-length file occupies no range: skip, never GET
        }
        let f_end = f.offset + f.length;
        if f.offset >= span_end || f_end <= span_start {
            continue;
        }
        let from = span_start.max(f.offset) - f.offset;
        let to = span_end.min(f_end) - f.offset - 1; // inclusive
        reqs.push((file_url(base, meta, f), from, to, f.length));
    }
    reqs
}

/// Assemble a run of contiguous pieces from the HTTP mirror.
///
/// The span is a window over the concatenated file stream, so it can straddle
/// several files, and BEP 19 forces one request per file it touches. Those
/// requests go out together rather than one after another: `buffered` keeps
/// the responses in stream order, which is what lets the pieces be cut back
/// out of the assembled buffer afterwards.
async fn fetch_span(
    client: &reqwest::Client,
    meta: &TorrentMeta,
    base: &str,
    first: u32,
    last: u32,
) -> Result<Vec<u8>, String> {
    let (span_start, span_end) = span_byte_range(meta, first, last);
    let want = span_end - span_start;
    let reqs = span_file_ranges(meta, base, first, last);

    let chunks: Vec<Result<Vec<u8>, String>> = futures::stream::iter(reqs.into_iter().map(
        |(url, from, to, len)| {
            let client = client.clone();
            async move { fetch_range(&client, &url, from, to, len).await }
        },
    ))
    .buffered(SPAN_FILE_PARALLEL)
    .collect()
    .await;

    let mut out: Vec<u8> = Vec::with_capacity(want as usize);
    for c in chunks {
        out.extend_from_slice(&c?);
    }

    if out.len() as u64 != want {
        return Err(format!(
            "pieces {}..={} assembled to {} bytes, expected {}",
            first,
            last,
            out.len(),
            want
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
    // Availability is deliberately NOT incremented anywhere for a webseed:
    // the swarm availability figure describes peers, and an HTTP mirror is
    // not one.
    let num_pieces = t.meta.num_pieces();
    let mut done = 0u32;
    let mut budget = MAX_PIECES_PER_CLAIM;

    while budget > 0 {
        if !wants_webseed(t) {
            break;
        }
        let picker = match t.picker.get() {
            Some(p) => p,
            None => break,
        };

        // Reserve a contiguous run of pieces. The picker chooses the head; the
        // run then walks forward over pieces we neither hold nor have already
        // reserved, until the byte target is met. The lock is released before
        // any await: holding a std Mutex across .await would poison the whole
        // download path.
        let run: Vec<u32> = {
            let mut p = picker.lock().unwrap();
            // NOT pick_piece: rarest-first tie-breaks at random, and every
            // piece is equally rare on a torrent with no peers. A random start
            // truncates the run, because a run only grows forwards.
            let first = match p.first_missing() {
                Some(i) => i,
                None => break,
            };
            let mut run = Vec::new();
            let mut bytes = t.meta.piece_size(first) as u64;
            p.start_piece(first, t.meta.piece_size(first), BLOCK_SIZE);
            run.push(first);

            let mut next = first + 1;
            while bytes < SPAN_TARGET_BYTES
                && run.len() < SPAN_MAX_PIECES
                && (run.len() as u32) < budget
                && next < num_pieces
            {
                if p.has_piece(next) || p.is_pending(next) {
                    break;
                }
                let sz = t.meta.piece_size(next);
                p.start_piece(next, sz, BLOCK_SIZE);
                run.push(next);
                bytes += sz as u64;
                next += 1;
            }
            run
        };

        let first = run[0];
        let last = *run.last().unwrap();

        // Rotate over the mirrors so a multi-host url-list spreads load, and
        // fall through to the next one when a host is down.
        let mut data = None;
        let mut last_err = String::new();
        let t_fetch = std::time::Instant::now();
        N_SPANS.fetch_add(1, Ordering::Relaxed);
        N_REQS.fetch_add(
            span_file_ranges(&t.meta, "x", first, last).len() as u64,
            Ordering::Relaxed,
        );
        for k in 0..t.meta.url_list.len() {
            let base = &t.meta.url_list[(first as usize + k) % t.meta.url_list.len()];
            match fetch_span(client, &t.meta, base, first, last).await {
                Ok(d) => {
                    data = Some(d);
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        add_ns(&T_FETCH, t_fetch);
        let data = match data {
            Some(d) => d,
            None => {
                let mut p = picker.lock().unwrap();
                for &i in &run {
                    p.cancel_piece(i);
                }
                return Err(last_err);
            }
        };
        N_BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);

        // Cut the span back into pieces and commit them one by one, each
        // through the same block-level checks and completion sequence a peer
        // download uses.
        let mut off = 0usize;
        for (n, &index) in run.iter().enumerate() {
            let psz = t.meta.piece_size(index) as usize;
            let piece_bytes = &data[off..off + psz];
            off += psz;

            let t_feed = std::time::Instant::now();
            let complete = {
                let mut p = picker.lock().unwrap();
                let mut c = false;
                let mut b = 0u32;
                while (b as usize) < piece_bytes.len() {
                    let end = (b as usize + BLOCK_SIZE as usize).min(piece_bytes.len());
                    c = p.receive_block(index, b, &piece_bytes[b as usize..end]);
                    b = end as u32;
                }
                c
            };
            add_ns(&T_FEED, t_feed);
            if !complete {
                let mut p = picker.lock().unwrap();
                for &i in &run[n..] {
                    p.cancel_piece(i);
                }
                return Err(format!("piece {} refused by the picker", index));
            }

            let piece_data = { picker.lock().unwrap().take_piece_data(index) };
            let piece_data = match piece_data {
                Some(d) => d,
                None => continue,
            };
            let t_commit = std::time::Instant::now();
            let ok = crate::peer::download::commit_piece(t, disk, index, piece_data).await;
            add_ns(&T_COMMIT, t_commit);
            if ok {
                done += 1;
                budget = budget.saturating_sub(1);
            } else {
                // SHA1 mismatch or write error: commit_piece already released
                // this piece. Release the rest of the run too — a mirror
                // serving wrong bytes is a real failure, not a retry loop.
                let mut p = picker.lock().unwrap();
                for &i in &run[n + 1..] {
                    p.cancel_piece(i);
                }
                return Err(format!("piece {} failed verification", index));
            }
        }
    }
    Ok(done)
}

/// One worker: find an unclaimed torrent that wants webseed help, drive it,
/// release it.
async fn worker(mgr: Arc<TorrentManager>, disk: Arc<DiskManager>, client: reqwest::Client) {
    loop {
        let t_pick = std::time::Instant::now();
        let candidate = next_candidate(&mgr);
        add_ns(&T_PICK, t_pick);
        let t = match candidate {
            Some(t) => t,
            None => {
                // The scanner refills every 500 ms; waiting longer than
                // that just idles a worker.
                let t_wait = std::time::Instant::now();
                tokio::time::sleep(Duration::from_millis(500)).await;
                add_ns(&T_WAIT, t_wait);
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

/// Work waiting to be claimed, filled by the scanner, drained by the workers.
static QUEUE: std::sync::OnceLock<std::sync::Mutex<VecDeque<InfoHash>>> =
    std::sync::OnceLock::new();

fn queue() -> &'static std::sync::Mutex<VecDeque<InfoHash>> {
    QUEUE.get_or_init(|| std::sync::Mutex::new(VecDeque::new()))
}

/// Refill the queue when it runs low, in ONE walk of the catalogue.
///
/// Every worker used to walk all 243k torrents itself, once per torrent it
/// claimed, taking the picker mutex on each of the ~2000 downloading ones for
/// an `is_complete()` in O(pieces). At 48 workers that walk became the engine's
/// main occupation: tripling the workers tripled the CPU (87% -> 223%) and left
/// throughput exactly where it was. One scanner amortises the walk over a whole
/// batch, and it runs on a blocking thread because it is a long synchronous
/// scan that has no business sitting on a runtime worker.
async fn scanner(mgr: Arc<TorrentManager>) {
    loop {
        let queued = queue().lock().map(|q| q.len()).unwrap_or(0);
        if queued < QUEUE_LOW {
            let mgr2 = mgr.clone();
            let found = tokio::task::spawn_blocking(move || {
                let now = now_secs();
                mgr2.collect_torrents(QUEUE_TARGET, |t| {
                    let ih = t.info_hash;
                    if let Some(b) = backoff().get(&ih) {
                        if b.1 > now {
                            return false;
                        }
                    }
                    if claimed().contains(&ih) {
                        return false;
                    }
                    wants_webseed(t)
                })
            })
            .await
            .unwrap_or_default();

            if let Ok(mut q) = queue().lock() {
                for ih in found {
                    q.push_back(ih);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Take the next piece of work off the queue, claiming it on the way out.
fn next_candidate(mgr: &TorrentManager) -> Option<Arc<TorrentState>> {
    loop {
        let ih = {
            let mut q = queue().lock().ok()?;
            q.pop_front()?
        };
        if !claimed().insert(ih) {
            continue; // another worker got there first
        }
        match mgr.get(&ih) {
            Some(t) if wants_webseed(&t) => return Some(t),
            _ => {
                // Gone or finished between the scan and now.
                claimed().remove(&ih);
                continue;
            }
        }
    }
}

/// Log the phase breakdown once a minute, then reset it. Deltas rather than
/// totals: a running average hides a regime change.
async fn reporter() {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let w = T_WAIT.swap(0, Ordering::Relaxed) / 1_000_000;
        let p = T_PICK.swap(0, Ordering::Relaxed) / 1_000_000;
        let f = T_FETCH.swap(0, Ordering::Relaxed) / 1_000_000;
        let d = T_FEED.swap(0, Ordering::Relaxed) / 1_000_000;
        let c = T_COMMIT.swap(0, Ordering::Relaxed) / 1_000_000;
        let spans = N_SPANS.swap(0, Ordering::Relaxed);
        let reqs = N_REQS.swap(0, Ordering::Relaxed);
        let bytes = N_BYTES.swap(0, Ordering::Relaxed);
        info!(
            "[webseed] 60s: wait={}ms pick={}ms fetch={}ms feed={}ms commit={}ms | \
             spans={} reqs={} ({:.1}/s) MB={:.1} ({:.2} MB/s) | ms_per_req={:.0}",
            w,
            p,
            f,
            d,
            c,
            spans,
            reqs,
            reqs as f64 / 60.0,
            bytes as f64 / 1e6,
            bytes as f64 / 60.0 / 1e6,
            if reqs > 0 { f as f64 / reqs as f64 } else { 0.0 }
        );
    }
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
    {
        let mgr = mgr.clone();
        tokio::spawn(async move { scanner(mgr).await });
    }
    tokio::spawn(async move { reporter().await });
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

    /// A single-piece span covers exactly that piece.
    #[test]
    fn span_of_one_piece_is_that_piece() {
        let mut m = meta(true, "i", vec![("f", 40000)]);
        m.piece_length = 16384;
        m.num_pieces = 3;
        m.total_size = 40000;
        assert_eq!(span_byte_range(&m, 1, 1), (16384, 32768));
    }

    /// A multi-piece span runs from the first piece's start to the last one's
    /// end — the whole point of the batch.
    #[test]
    fn span_covers_first_start_to_last_end() {
        let mut m = meta(true, "i", vec![("f", 40000)]);
        m.piece_length = 16384;
        m.num_pieces = 3;
        m.total_size = 40000;
        assert_eq!(span_byte_range(&m, 0, 2), (0, 40000));
    }

    /// The last piece is short, and the span must stop at the real end of the
    /// torrent rather than at a rounded piece boundary — asking archive.org for
    /// bytes past EOF is how you earn a 416 on every final span.
    #[test]
    fn span_stops_at_the_short_tail_piece() {
        let mut m = meta(true, "i", vec![("f", 40000)]);
        m.piece_length = 16384;
        m.num_pieces = 3;
        m.total_size = 40000;
        let (_, end) = span_byte_range(&m, 2, 2);
        assert_eq!(end, 40000);
        assert!(end < 3 * 16384);
    }

    /// A span that straddles three files resolves to three requests, each
    /// clipped to its own file's coordinates. Getting these offsets wrong is
    /// how a span silently assembles corrupt bytes that then fail SHA1.
    #[test]
    fn span_splits_into_one_request_per_file_touched() {
        let mut m = meta(true, "it", vec![("a", 100), ("b", 100), ("c", 100)]);
        m.piece_length = 150;
        m.num_pieces = 2;
        m.total_size = 300;
        // pieces 0..=1 cover bytes 0..300 = all three files
        let r = span_file_ranges(&m, "http://h/", 0, 1);
        assert_eq!(r.len(), 3);
        assert_eq!((r[0].1, r[0].2), (0, 99));
        assert_eq!((r[1].1, r[1].2), (0, 99));
        assert_eq!((r[2].1, r[2].2), (0, 99));
    }

    /// A span covering only the middle of the stream must not request the
    /// files it does not touch, and must clip the ones it partially covers.
    #[test]
    fn span_clips_partial_files_and_skips_untouched_ones() {
        let mut m = meta(true, "it", vec![("a", 100), ("b", 100), ("c", 100)]);
        m.piece_length = 50;
        m.num_pieces = 6;
        m.total_size = 300;
        // piece 2 = bytes 100..150 -> file b only, its first 50 bytes
        let r = span_file_ranges(&m, "http://h/", 2, 2);
        assert_eq!(r.len(), 1);
        assert!(r[0].0.ends_with("/b"));
        assert_eq!((r[0].1, r[0].2), (0, 49));
    }

    /// A zero-length file sits at an offset but owns no bytes; requesting a
    /// range on it would be a guaranteed 416.
    #[test]
    fn zero_length_files_are_never_requested() {
        let mut m = meta(true, "it", vec![("a", 100), ("empty", 0), ("b", 100)]);
        m.piece_length = 200;
        m.num_pieces = 1;
        m.total_size = 200;
        let r = span_file_ranges(&m, "http://h/", 0, 0);
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| !x.0.ends_with("/empty")));
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
