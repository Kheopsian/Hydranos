//! Two different torrents naming the same file on disk.
//!
//! The engine writes a torrent's pieces wherever its metainfo says its files
//! are, and nothing stopped a second torrent from naming files a first one
//! already held. Same folder name, same file names, different bytes -- a
//! re-upload that kept the release name, a REPACK that did not rename, an
//! encode published twice under one title. The second torrent found files at
//! its paths, rechecked them, failed the pieces that differed, fetched them,
//! and wrote them over the first torrent's file. The first never rehashes what
//! it has already verified, so it went on seeding the mix: complete by its own
//! account, rejected by every peer, and played back broken by whatever read
//! the file. No error, no log line, no counter.
//!
//! Measured on a 1.09M-torrent catalogue (reported 2026-10-07): 14 paths named
//! by two or more torrents with different sizes, and on every one of them at
//! most one torrent's pieces matched the disk. One had BOTH torrents broken --
//! the ping-pong of two rechecks -- and one was a 25-episode season seeded
//! broken for seven weeks.
//!
//! The rule this module enforces: nothing writes into a file another torrent
//! has claimed unless the bytes are proven identical. Proven means hashed, not
//! sized: two `.nfo` files of the same length can differ, and `mkvpropedit`
//! rewrites a 25 GB file in place without changing its size by one byte.
//!
//! What it does about an incoming torrent, per file, before anything is
//! written:
//!
//! - nobody names the path: nothing to do;
//! - another torrent names it with the same pieces and the same file list
//!   (cross-seeding the very same bytes): shared, as it always was;
//! - another torrent names it with another size: the two cannot both be
//!   right, so the incoming torrent gets a folder of its own;
//! - another torrent names it with the same size: unknown until hashed, so the
//!   incoming torrent also gets a folder of its own, and every file whose
//!   pieces verify against the existing file is hardlinked into it -- the
//!   disk cost is that of the files that really differ, nothing more.
//!
//! The proof reads the EXISTING files and hashes them against the incoming
//! torrent's pieces before a single link is made, so a restart in the middle
//! leaves no unproven link behind: at worst a paused torrent with an empty
//! folder, which downloads what it needs when resumed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use typhon_engine::torrent::meta::{TorrentMeta, TorrentState};

use crate::api::AppState;

/// The anomaly kind, as `/api/health/anomalies` reports it.
pub const PATH_COLLISION: &str = "path_collision";

/// What to do with an incoming torrent whose files another torrent already
/// names with other bytes (`[dedup] path_collision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// A folder of its own next to the one it asked for, with the files
    /// proven identical hardlinked in. Nobody is refused and nothing is
    /// overwritten, which is why it is the default.
    #[default]
    Isolate,
    /// Refuse the add and say why. For an operator who would rather pick the
    /// release by hand.
    Refuse,
    /// Every version before this one: share the path and let the recheck
    /// overwrite whatever differs. Kept for an operator who relies on it
    /// knowingly; logged and reported every time it lets one through.
    Legacy,
}

/// One torrent already naming some of the paths being checked.
#[derive(Debug, Clone)]
pub struct Claimant {
    pub info_hash: String,
    /// Same pieces and same file list: the same bytes under the same names.
    /// Sharing a path with it is what cross-seeding is.
    pub identical: bool,
    /// The checked paths it names, with the size IT expects there.
    pub files: Vec<(PathBuf, u64)>,
}

/// The paths of one torrent that other torrents name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overlap {
    /// Named by another torrent with another size: both cannot be right.
    pub conflicts: Vec<PathBuf>,
    /// Same size, another torrent: maybe the same bytes, maybe not.
    pub unproven: Vec<PathBuf>,
    /// The non-identical torrents involved.
    pub with: Vec<String>,
}

impl Overlap {
    pub fn is_clear(&self) -> bool {
        self.conflicts.is_empty() && self.unproven.is_empty()
    }
}

/// How two paths compare on this platform: NTFS does not tell `A.mkv` from
/// `a.mkv`, so two torrents differing only by case name the same file there.
pub fn path_key(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(p.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        p.to_path_buf()
    }
}

/// Which of `mine` (path, size this torrent expects) other torrents name.
///
/// A path named by one claimant with another size is a conflict even if a
/// second claimant agrees on the size: the file can only hold one of them.
pub fn overlap(mine: &[(PathBuf, u64)], claimants: &[Claimant]) -> Overlap {
    let want: HashMap<PathBuf, u64> = mine.iter().map(|(p, l)| (path_key(p), *l)).collect();
    let mut conflicts: HashSet<PathBuf> = HashSet::new();
    let mut unproven: HashSet<PathBuf> = HashSet::new();
    let mut with: Vec<String> = Vec::new();
    for c in claimants.iter().filter(|c| !c.identical) {
        let mut touched = false;
        for (p, theirs) in &c.files {
            let k = path_key(p);
            let Some(&len) = want.get(&k) else { continue };
            touched = true;
            if len == *theirs {
                unproven.insert(k);
            } else {
                conflicts.insert(k);
            }
        }
        if touched && !with.contains(&c.info_hash) {
            with.push(c.info_hash.clone());
        }
    }
    let mut unproven: Vec<PathBuf> = unproven
        .into_iter()
        .filter(|p| !conflicts.contains(p))
        .collect();
    let mut conflicts: Vec<PathBuf> = conflicts.into_iter().collect();
    conflicts.sort();
    unproven.sort();
    Overlap {
        conflicts,
        unproven,
        with,
    }
}

/// What to do with an incoming torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing in the way, or only torrents holding the very same bytes.
    Proceed,
    /// Where it asked, but held until the files listed (relative to the
    /// torrent's root) are proven: present on disk, named by no torrent, and
    /// linked from somewhere else -- the *arr library, typically. A recheck
    /// that fetched over them would rewrite that other copy too.
    ProveInPlace(Vec<PathBuf>),
    /// In a folder of its own.
    Isolate,
    Refuse(String),
}

/// The decision, from what was found. Pure: every input is a fact gathered
/// before, so the rule can be read and tested on its own.
///
/// A `seed_mode` add (`skip_checking`) with no size conflict proceeds: the
/// caller said the data is right, the torrent writes nothing until a recheck,
/// and a recheck of a torrent sharing files goes through `guarded_recheck`.
/// A size conflict is not something skip_checking can vouch for -- the file
/// cannot be two sizes -- so it is isolated like any other.
pub fn decide(ov: &Overlap, foreign: &[PathBuf], mode: Mode, seed_mode: bool) -> Decision {
    if ov.is_clear() {
        return if foreign.is_empty() || seed_mode {
            Decision::Proceed
        } else {
            Decision::ProveInPlace(foreign.to_vec())
        };
    }
    if seed_mode && ov.conflicts.is_empty() {
        return Decision::Proceed;
    }
    match mode {
        Mode::Legacy => Decision::Proceed,
        Mode::Refuse => {
            Decision::Refuse(format!(
            "{} file(s) of this torrent are already another torrent's with other bytes ({}{}); \
             adding it there would overwrite them",
            ov.conflicts.len() + ov.unproven.len(),
            ov.conflicts
                .iter()
                .chain(&ov.unproven)
                .take(3)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if ov.conflicts.len() + ov.unproven.len() > 3 { ", ..." } else { "" },
        ))
        }
        Mode::Isolate => Decision::Isolate,
    }
}

/// The save path an isolated torrent gets: a folder next to the one it asked
/// for, named after what it would have created there plus its hash, so two
/// releases of one title sit side by side and say which is which.
pub fn isolated_save_path(save_path: &str, name: &str, info_hash: &str) -> PathBuf {
    let top = Path::new(name)
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let short = &info_hash[..info_hash.len().min(8)];
    Path::new(save_path).join(format!("{top} [{short}]"))
}

/// The directory a torrent's file paths hang off: the save path. Its paths
/// start with the torrent's folder (`name/<file>`, or wherever a file was
/// renamed to) or, for a single file, are the file's own name.
pub fn root_of(_meta: &TorrentMeta, save_path: &Path) -> PathBuf {
    save_path.to_path_buf()
}

/// Every file of `meta` under `save_path`: (absolute path, path below the
/// save path, size).
pub fn files_of(meta: &TorrentMeta, save_path: &Path) -> Vec<(PathBuf, PathBuf, u64)> {
    meta.files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let rel = meta.disk_rel(i);
            (save_path.join(&rel), rel, f.length)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Adds in flight
// ---------------------------------------------------------------------------

fn inflight() -> &'static Mutex<Vec<PathBuf>> {
    static S: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

/// A path held by an add between its check and the moment the engine holds
/// the torrent. Released when dropped.
#[derive(Debug)]
pub struct Reservation(PathBuf);

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut g = inflight().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = g.iter().position(|p| *p == self.0) {
            g.swap_remove(i);
        }
    }
}

/// How long an add waits behind another one on the same branch before going
/// ahead anyway. Each holds the path for a scan and an insert, well under a
/// second; this is a safety valve, not a timeout anyone should reach.
const RESERVE_PATIENCE: Duration = Duration::from_secs(30);

/// Hold `root` against every other add on the same branch.
///
/// The check reads what the engines hold and the add makes the engine hold
/// one more: two adds naming the same folder at the same instant -- the same
/// release grabbed from two trackers, which is exactly when it happens --
/// would both pass a check that ran before either was added. Adds on
/// unrelated paths never wait for each other.
pub fn reserve(root: &Path) -> Reservation {
    let deadline = Instant::now() + RESERVE_PATIENCE;
    loop {
        {
            let mut g = inflight().lock().unwrap_or_else(|e| e.into_inner());
            let busy = g.iter().any(|p| p.starts_with(root) || root.starts_with(p));
            if !busy || Instant::now() >= deadline {
                if busy {
                    tracing::warn!(path = %root.display(), "path claim: waited too long on another add, going ahead");
                }
                g.push(root.to_path_buf());
                return Reservation(root.to_path_buf());
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Finding the claimants
// ---------------------------------------------------------------------------

/// Whether two torrents hold the same bytes under the same names.
fn identical(a: &[u8], b: &[u8]) -> bool {
    if !crate::dedup::same_content(a, b) {
        return false;
    }
    match (crate::dedup::layout(a), crate::dedup::layout(b)) {
        (Some(x), Some(y)) => x.files == y.files && x.multi_file == y.multi_file,
        _ => false,
    }
}

/// The torrents of every engine that name one of `mine` (absolute paths
/// hanging off `save_path`, first component `top`), with whether each holds
/// the very same bytes as `bytes`. `skip` leaves out the torrent being
/// checked and anything else the caller says is not a claimant.
pub fn claimants(
    state: &AppState,
    bytes: &[u8],
    save_path: &Path,
    top: Option<&std::ffi::OsStr>,
    mine: &[(PathBuf, u64)],
    skip: &(dyn Fn(&TorrentState) -> bool + Sync),
) -> Vec<Claimant> {
    let keys: HashSet<PathBuf> = mine.iter().map(|(p, _)| path_key(p)).collect();
    let hits = crate::jobsrun::scan_claims(state, save_path, top, &keys, skip);
    let mut out: Vec<Claimant> = Vec::with_capacity(hits.len());
    for (other, files) in hits {
        let hash = typhon_engine::torrent::hex_encode(&other.meta.info_hash);
        if let Some(c) = out.iter_mut().find(|c| c.info_hash == hash) {
            // The same hash in a second engine: one claimant, its files once.
            for f in files {
                if !c.files.contains(&f) {
                    c.files.push(f);
                }
            }
            continue;
        }
        let blob = {
            let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            store.torrent_blob(&hash).ok().flatten()
        };
        // No metainfo to compare: assume the worst. Treating it as identical
        // would share a path on a guess.
        let same = blob.map(|b| identical(bytes, &b)).unwrap_or(false);
        out.push(Claimant {
            info_hash: hash,
            identical: same,
            files,
        });
    }
    out
}

/// Files of an incoming torrent present on disk, named by no torrent, with
/// another name somewhere (a hardlink count above one).
///
/// A download that rewrote them would rewrite that other name too. With a
/// single link the file is only ever this path, and a re-add finding its own
/// data is the ordinary case: nothing to protect.
pub fn foreign_links(mine: &[(PathBuf, PathBuf, u64)], claimed: &HashSet<PathBuf>) -> Vec<PathBuf> {
    mine.iter()
        .filter(|(abs, _, _)| !claimed.contains(&path_key(abs)))
        .filter(|(abs, _, _)| abs.is_file() && crate::jobs::link_count(abs) > 1)
        .map(|(_, rel, _)| rel.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// The proof
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn read_at(f: &std::fs::File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(windows)]
fn read_at(f: &std::fs::File, buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = f.seek_read(&mut buf[done..], off)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
        off += n as u64;
    }
    Ok(())
}

/// Which of `candidates` (paths relative to the torrent root, as in
/// `meta.files`) hold exactly the bytes this torrent describes, read from
/// under `src_root`.
///
/// A file is proven when EVERY piece overlapping it verifies. A piece that
/// also covers a file outside `candidates` cannot be read from here and
/// counts as failed: the file it shares stays unproven and is downloaded,
/// which costs bytes and never correctness. Zero-length files hold nothing to
/// hash and are proven when present.
///
/// `verify(piece, data)` is the torrent's own check (SHA-1 for v1, the merkle
/// root for v2); None -- no hash table -- fails the piece.
pub fn prove(
    meta: &TorrentMeta,
    src_root: &Path,
    candidates: &HashSet<PathBuf>,
    verify: &dyn Fn(u32, &[u8]) -> Option<bool>,
) -> HashSet<PathBuf> {
    let plen = meta.piece_length as u64;
    let mut wanted: HashMap<PathBuf, (u32, u32)> = HashMap::new();
    let mut proven: HashSet<PathBuf> = HashSet::new();
    for (i, f) in meta.files.iter().enumerate() {
        let path = meta.disk_rel(i);
        if !candidates.contains(&path) {
            continue;
        }
        if f.length == 0 {
            if src_root.join(&path).is_file() {
                proven.insert(path);
            }
            continue;
        }
        if plen == 0 {
            continue;
        }
        let first = (f.offset / plen) as u32;
        let last = ((f.offset + f.length - 1) / plen) as u32;
        wanted.insert(path, (first, last));
    }
    let mut pieces: Vec<u32> = wanted.values().flat_map(|&(a, b)| a..=b).collect();
    pieces.sort_unstable();
    pieces.dedup();

    let mut open: HashMap<PathBuf, Option<std::fs::File>> = HashMap::new();
    let mut good: HashSet<u32> = HashSet::new();
    'piece: for p in pieces {
        let size = meta.piece_size(p);
        let mut data: Vec<u8> = Vec::with_capacity(size as usize);
        for op in meta.map_block(p, 0, size) {
            if op.pad {
                data.resize(data.len() + op.length as usize, 0);
                continue;
            }
            if !candidates.contains(&op.path) {
                continue 'piece;
            }
            let file = open
                .entry(op.path.clone())
                .or_insert_with(|| std::fs::File::open(src_root.join(&op.path)).ok());
            let Some(file) = file.as_ref() else {
                continue 'piece;
            };
            let at = data.len();
            data.resize(at + op.length as usize, 0);
            if read_at(file, &mut data[at..], op.file_offset).is_err() {
                continue 'piece;
            }
        }
        if verify(p, &data) == Some(true) {
            good.insert(p);
        }
    }
    for (path, (a, b)) in wanted {
        if (a..=b).all(|p| good.contains(&p)) {
            proven.insert(path);
        }
    }
    proven
}

/// `prove` against a torrent the engine holds.
///
/// Checked against the SHA-1 table of `metainfo` -- the bytes the caller
/// already has in hand -- rather than through the engine, which loads its own
/// copy from the store and keeps it for the torrent's life. Only a v2-only
/// torrent, which has no SHA-1 table, goes through the engine's merkle check.
pub fn prove_torrent(
    t: &TorrentState,
    metainfo: Option<&[u8]>,
    src_root: &Path,
    candidates: &HashSet<PathBuf>,
) -> HashSet<PathBuf> {
    let table = metainfo.and_then(crate::dedup::pieces);
    let check = |p: u32, d: &[u8]| -> Option<bool> {
        match table {
            Some(tab) => {
                use sha1::{Digest, Sha1};
                let want = tab.get(p as usize * 20..p as usize * 20 + 20)?;
                Some(Sha1::digest(d).as_slice() == want)
            }
            None => t.verify_piece(p, d),
        }
    };
    prove(&t.meta, src_root, candidates, &check)
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One thing this module did, or let through, for the anomaly route.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Event {
    pub at: i64,
    pub engine: String,
    pub info_hash: String,
    pub name: String,
    /// `isolated`, `refused`, `shared` (legacy mode let it through),
    /// `held` (a proof failed: paused, nothing written), `proven`.
    pub action: String,
    pub detail: String,
}

const EVENTS_KEPT: usize = 500;

fn events() -> &'static Mutex<VecDeque<Event>> {
    static E: OnceLock<Mutex<VecDeque<Event>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Log it and keep it for the anomaly route. In memory: a restart forgets the
/// list, the log line stays.
pub fn record(engine: &str, info_hash: &str, name: &str, action: &str, detail: String) {
    tracing::warn!(engine, hash = %info_hash, name, action, "path collision: {detail}");
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut g = events().lock().unwrap_or_else(|e| e.into_inner());
    if g.len() >= EVENTS_KEPT {
        g.pop_front();
    }
    g.push_back(Event {
        at,
        engine: engine.to_string(),
        info_hash: info_hash.to_string(),
        name: name.to_string(),
        action: action.to_string(),
        detail,
    });
}

/// What `record` kept, oldest first.
pub fn recent() -> Vec<Event> {
    events()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// The add
// ---------------------------------------------------------------------------

/// What `check_add` settled for an incoming torrent.
#[derive(Debug)]
pub struct AddPlan {
    /// Where it goes: the asked save path, or its own folder.
    pub save_path: String,
    /// Added paused whatever the caller asked, until `finish` has run.
    pub hold: bool,
    /// Isolated: files to prove from the original location and link in.
    pub isolate_from: Option<(PathBuf, HashSet<PathBuf>)>,
    /// Where it asked, held until these files are proven in place.
    pub prove_in_place: Option<HashSet<PathBuf>>,
    pub note: Option<String>,
    /// Held until the engine holds the torrent: dropped at the end of the add.
    /// Never read; owning it is the point.
    pub _reservation: Reservation,
}

/// Check an incoming torrent against every torrent already held, before the
/// store or the engine hear of it.
pub fn check_add(
    state: &AppState,
    bytes: &[u8],
    meta: &TorrentMeta,
    hash: &str,
    engine_id: &str,
    save_path: &str,
    seed_mode: bool,
    mode: Mode,
) -> Result<AddPlan, String> {
    let save = Path::new(save_path);
    let top = meta.common_root();
    let reservation = reserve(&save.join(top.clone().unwrap_or_default()));

    let all = files_of(meta, save);
    let mine: Vec<(PathBuf, u64)> = all.iter().map(|(a, _, l)| (a.clone(), *l)).collect();
    let ih = meta.info_hash;
    // Its own copies in other engines read the same bytes by definition.
    let skip = move |o: &TorrentState| o.meta.info_hash == ih;
    let found = claimants(state, bytes, save, top.as_deref(), &mine, &skip);
    let ov = overlap(&mine, &found);
    let claimed: HashSet<PathBuf> = found
        .iter()
        .flat_map(|c| c.files.iter().map(|(p, _)| path_key(p)))
        .collect();
    let foreign = if ov.is_clear() {
        foreign_links(&all, &claimed)
    } else {
        Vec::new()
    };

    let mut plan = AddPlan {
        save_path: save_path.to_string(),
        hold: false,
        isolate_from: None,
        prove_in_place: None,
        note: None,
        _reservation: reservation,
    };
    let rel_of: HashMap<PathBuf, PathBuf> = all
        .iter()
        .map(|(a, r, _)| (path_key(a), r.clone()))
        .collect();
    match decide(&ov, &foreign, mode, seed_mode) {
        Decision::Proceed if ov.is_clear() => {}
        // skip_checking over same-size files of another torrent: a cross-seed
        // cut into other pieces, the usual case (thousands in a library that
        // uploads what it already seeds). Logged, not an anomaly: the caller
        // vouched for the bytes, and a size conflict never gets here.
        Decision::Proceed if seed_mode => {
            tracing::info!(
                engine = engine_id,
                hash,
                files = ov.unproven.len(),
                "path claim: skip_checking over files another torrent holds at the same size"
            );
        }
        Decision::Proceed => {
            record(engine_id, hash, &meta.name, "shared", format!(
                "legacy mode: {} file(s) named by {} other torrent(s) with other bytes, {} of another size; \
                 a recheck may overwrite them",
                ov.conflicts.len() + ov.unproven.len(),
                ov.with.len(),
                ov.conflicts.len()
            ));
        }
        Decision::ProveInPlace(rel) => {
            plan.hold = true;
            plan.note = Some(format!(
                "held until {} hardlinked file(s) are proven",
                rel.len()
            ));
            plan.prove_in_place = Some(rel.into_iter().collect());
        }
        Decision::Refuse(why) => {
            record(engine_id, hash, &meta.name, "refused", why.clone());
            return Err(why);
        }
        Decision::Isolate => {
            let to = isolated_save_path(save_path, &meta.name, hash);
            // The files worth proving are the ones another torrent holds at
            // the right size; a size conflict is a different file by
            // definition.
            let candidates: HashSet<PathBuf> = ov
                .unproven
                .iter()
                .filter_map(|p| rel_of.get(p).cloned())
                .collect();
            record(
                engine_id,
                hash,
                &meta.name,
                "isolated",
                format!(
                    "{} file(s) already another torrent's ({} of another size), placed in {}",
                    ov.conflicts.len() + ov.unproven.len(),
                    ov.conflicts.len(),
                    to.display()
                ),
            );
            plan.save_path = to.to_string_lossy().into_owned();
            plan.hold = true;
            plan.note = Some(format!("isolated in {}", to.display()));
            plan.isolate_from = Some((root_of(meta, save), candidates));
        }
    }
    Ok(plan)
}

/// After the engine holds an isolated or held torrent: prove, link, recheck,
/// and release it if the caller wanted it running.
///
/// Runs on a thread of its own -- it reads gigabytes -- with the runtime
/// handle the engine's recheck needs. Under test it runs inline, so a test
/// sees the outcome when the add returns.
pub fn finish(
    state: &AppState,
    engine_id: &str,
    hash: &str,
    bytes: &[u8],
    plan: &AddPlan,
    resume: bool,
) {
    let job = Job {
        metainfo: Some(bytes.to_vec()),
        state: state.clone(),
        engine_id: engine_id.to_string(),
        hash: hash.to_string(),
        isolate_from: plan.isolate_from.clone(),
        prove_in_place: plan.prove_in_place.clone(),
        resume,
        rt: tokio::runtime::Handle::try_current().ok(),
    };
    if cfg!(test) {
        job.run();
    } else {
        std::thread::Builder::new()
            .name("pathclaim-proof".into())
            .spawn(move || job.run())
            .ok();
    }
}

struct Job {
    metainfo: Option<Vec<u8>>,
    state: AppState,
    engine_id: String,
    hash: String,
    isolate_from: Option<(PathBuf, HashSet<PathBuf>)>,
    prove_in_place: Option<HashSet<PathBuf>>,
    resume: bool,
    rt: Option<tokio::runtime::Handle>,
}

impl Job {
    fn run(self) {
        let Some(engine) = self.state.engines.get(&self.engine_id) else {
            return;
        };
        let manager = engine.manager.clone();
        let Some(ih) = crate::store::hex20(&self.hash) else {
            return;
        };
        let Some(t) = manager.get(&ih) else { return };
        let name = t.meta.name.clone();
        let root = root_of(&t.meta, &t.save_path.read().clone());

        if let Some((src_root, candidates)) = &self.isolate_from {
            let proven = prove_torrent(&t, self.metainfo.as_deref(), src_root, candidates);
            let mut linked = 0usize;
            for rel in &proven {
                let (from, to) = (src_root.join(rel), root.join(rel));
                if to.exists() {
                    continue;
                }
                if let Some(parent) = to.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::hard_link(&from, &to).is_ok() {
                    linked += 1;
                }
            }
            if !candidates.is_empty() {
                record(
                    &self.engine_id,
                    &self.hash,
                    &name,
                    "proven",
                    format!(
                        "{} of {} same-size file(s) proven identical and linked, {} to download",
                        linked,
                        candidates.len(),
                        candidates.len() - linked
                    ),
                );
            }
            self.recheck_and_release(&manager, &ih);
            return;
        }

        if let Some(files) = &self.prove_in_place {
            let proven = prove_torrent(&t, self.metainfo.as_deref(), &root, files);
            if proven.len() == files.len() {
                self.recheck_and_release(&manager, &ih);
            } else {
                let bad = files.len() - proven.len();
                let msg = format!(
                    "{bad} file(s) on disk differ from this torrent and are hardlinked elsewhere; \
                     held paused rather than overwrite them"
                );
                t.mark_write_error(&msg);
                record(&self.engine_id, &self.hash, &name, "held", msg);
            }
        }
    }

    fn recheck_and_release(
        &self,
        manager: &Arc<typhon_engine::torrent::TorrentManager>,
        ih: &[u8; 20],
    ) {
        let _enter = self.rt.as_ref().map(|h| h.enter());
        if self.rt.is_some() {
            let _ = manager.recheck(ih);
        }
        if self.resume {
            {
                let store = self.state.store.lock().unwrap_or_else(|e| e.into_inner());
                crate::store::log_store(
                    "set_paused",
                    store.set_paused(&self.hash, &self.engine_id, false),
                );
            }
            let _ = manager.start_torrent(ih);
        }
    }
}

// ---------------------------------------------------------------------------
// Rechecks of torrents already held
// ---------------------------------------------------------------------------

/// The files of `t` (relative to its root) that another torrent names with
/// other bytes.
pub fn protected_files(state: &AppState, t: &TorrentState) -> HashSet<PathBuf> {
    guarded_files(state, t, false)
}

/// `protected_files`, plus -- with `links` -- the files on disk no torrent
/// names that have another name somewhere (an *arr library's hardlink): the
/// same set an add proves before it lets the torrent write.
fn guarded_files(state: &AppState, t: &TorrentState, links: bool) -> HashSet<PathBuf> {
    let hash = typhon_engine::torrent::hex_encode(&t.meta.info_hash);
    let Some(bytes) = ({
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.torrent_blob(&hash).ok().flatten()
    }) else {
        return HashSet::new();
    };
    let save = t.save_path.read().clone();
    let all = files_of(&t.meta, &save);
    let mine: Vec<(PathBuf, u64)> = all.iter().map(|(a, _, l)| (a.clone(), *l)).collect();
    let top = t.meta.common_root();
    let ih = t.meta.info_hash;
    let skip = move |o: &TorrentState| o.meta.info_hash == ih;
    let found = claimants(state, &bytes, &save, top.as_deref(), &mine, &skip);
    let ov = overlap(&mine, &found);
    let rel_of: HashMap<PathBuf, PathBuf> = all
        .iter()
        .map(|(a, r, _)| (path_key(a), r.clone()))
        .collect();
    let mut out: HashSet<PathBuf> = ov
        .conflicts
        .iter()
        .chain(&ov.unproven)
        .filter_map(|p| rel_of.get(p).cloned())
        .collect();
    if links {
        let claimed: HashSet<PathBuf> = found
            .iter()
            .flat_map(|c| c.files.iter().map(|(p, _)| path_key(p)))
            .collect();
        out.extend(foreign_links(&all, &claimed));
    }
    out
}

/// Recheck a torrent without letting it fetch over a file another torrent
/// names with other bytes.
///
/// A recheck finds the pieces that do not verify and the download path
/// fetches them -- into whatever file the path names. When another torrent
/// names it too, that is the overwrite this module exists to stop, coming
/// back through the Verify button or an *arr's post-import check. So: with
/// no such file, the plain recheck. With one, the torrent is held, those
/// files are hashed first, and it is rechecked and released only if they all
/// verify; otherwise it stays paused in error, saying why, and writes nothing.
pub fn guarded_recheck(
    state: &AppState,
    engine_id: &str,
    t: &Arc<TorrentState>,
) -> Result<(), String> {
    let Some(engine) = state.engines.get(engine_id) else {
        return Err(format!("no engine {engine_id}"));
    };
    let protected = protected_files(state, t);
    if protected.is_empty() {
        return engine.manager.recheck(&t.info_hash);
    }
    let hash = typhon_engine::torrent::hex_encode(&t.info_hash);
    let was_running = !t.is_paused.load(std::sync::atomic::Ordering::Relaxed);
    if was_running {
        {
            let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            crate::store::log_store("set_paused", store.set_paused(&hash, engine_id, true));
        }
        let _ = engine.manager.stop_torrent(&t.info_hash);
    }
    let plan_files = protected;
    let metainfo = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.torrent_blob(&hash).ok().flatten()
    };
    let job = Job {
        metainfo,
        state: state.clone(),
        engine_id: engine_id.to_string(),
        hash,
        isolate_from: None,
        prove_in_place: Some(plan_files),
        resume: was_running,
        rt: tokio::runtime::Handle::try_current().ok(),
    };
    if cfg!(test) {
        job.run();
    } else {
        std::thread::Builder::new()
            .name("pathclaim-recheck".into())
            .spawn(move || job.run())
            .map_err(|e| format!("cannot start the check: {e}"))?;
    }
    Ok(())
}

/// Start a torrent without letting it download over a file another torrent
/// names, or one with another name on disk.
///
/// An add proves those files before it releases the torrent, and holds it
/// paused meanwhile -- but the proof lived in memory: after a restart, or on a
/// torrent left paused in error because the proof failed, Start went straight
/// to the engine and the download wrote into the other torrent's file. Start
/// now asks the same question an add does. A torrent that holds everything
/// writes nothing and starts at once; so does one with nothing to protect.
pub fn guarded_start(
    state: &AppState,
    engine_id: &str,
    t: &Arc<TorrentState>,
) -> Result<(), String> {
    let Some(engine) = state.engines.get(engine_id) else {
        return Err(format!("no engine {engine_id}"));
    };
    if t.seed_mode || crate::workers::holds_everything(t) {
        return engine.manager.start_torrent(&t.info_hash);
    }
    let guarded = guarded_files(state, t, true);
    if guarded.is_empty() {
        return engine.manager.start_torrent(&t.info_hash);
    }
    let hash = typhon_engine::torrent::hex_encode(&t.info_hash);
    // Paused until proven, in the store as in the engine: the job releases it.
    {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        crate::store::log_store("set_paused", store.set_paused(&hash, engine_id, true));
    }
    let _ = engine.manager.stop_torrent(&t.info_hash);
    let metainfo = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.torrent_blob(&hash).ok().flatten()
    };
    let job = Job {
        metainfo,
        state: state.clone(),
        engine_id: engine_id.to_string(),
        hash,
        isolate_from: None,
        prove_in_place: Some(guarded),
        resume: true,
        rt: tokio::runtime::Handle::try_current().ok(),
    };
    if cfg!(test) {
        job.run();
    } else {
        std::thread::Builder::new()
            .name("pathclaim-start".into())
            .spawn(move || job.run())
            .map_err(|e| format!("cannot start the check: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claimant(hash: &str, identical: bool, files: &[(&str, u64)]) -> Claimant {
        Claimant {
            info_hash: hash.to_string(),
            identical,
            files: files.iter().map(|(p, l)| (PathBuf::from(p), *l)).collect(),
        }
    }

    fn mine(files: &[(&str, u64)]) -> Vec<(PathBuf, u64)> {
        files.iter().map(|(p, l)| (PathBuf::from(p), *l)).collect()
    }

    #[test]
    fn nobody_naming_the_paths_is_clear() {
        let ov = overlap(
            &mine(&[("/d/P/a.mkv", 10)]),
            &[claimant("x", false, &[("/d/Q/a.mkv", 10)])],
        );
        assert!(ov.is_clear());
        assert!(ov.with.is_empty());
    }

    #[test]
    fn an_identical_torrent_is_not_in_the_way() {
        // Cross-seeding the same bytes under the same names is the point.
        let ov = overlap(
            &mine(&[("/d/P/a.mkv", 10)]),
            &[claimant("x", true, &[("/d/P/a.mkv", 10)])],
        );
        assert!(ov.is_clear());
    }

    #[test]
    fn another_size_is_a_conflict_and_the_same_size_is_unproven() {
        // The production case: the same folder and file names, an encode of
        // another size in all but one episode.
        let ov = overlap(
            &mine(&[
                ("/d/S02/E01.mkv", 2_000_000_000),
                ("/d/S02/E04.mkv", 1_600_000_000),
            ]),
            &[claimant(
                "other",
                false,
                &[
                    ("/d/S02/E01.mkv", 1_500_000_000),
                    ("/d/S02/E04.mkv", 1_600_000_000),
                ],
            )],
        );
        assert_eq!(ov.conflicts, vec![PathBuf::from("/d/S02/E01.mkv")]);
        assert_eq!(ov.unproven, vec![PathBuf::from("/d/S02/E04.mkv")]);
        assert_eq!(ov.with, vec!["other".to_string()]);
    }

    #[test]
    fn a_size_disagreement_wins_over_an_agreement() {
        // One claimant agrees on the size, another does not: the file holds
        // one of them at most, so it is a conflict.
        let ov = overlap(
            &mine(&[("/d/P/a.nfo", 7225)]),
            &[
                claimant("a", false, &[("/d/P/a.nfo", 7225)]),
                claimant("b", false, &[("/d/P/a.nfo", 18793)]),
            ],
        );
        assert_eq!(ov.conflicts, vec![PathBuf::from("/d/P/a.nfo")]);
        assert!(ov.unproven.is_empty());
        assert_eq!(ov.with.len(), 2);
    }

    #[test]
    fn the_decision_follows_the_mode() {
        let conflict = Overlap {
            conflicts: vec!["/d/P/a".into()],
            unproven: vec![],
            with: vec!["x".into()],
        };
        assert_eq!(
            decide(&conflict, &[], Mode::Isolate, false),
            Decision::Isolate
        );
        assert!(matches!(
            decide(&conflict, &[], Mode::Refuse, false),
            Decision::Refuse(_)
        ));
        assert_eq!(
            decide(&conflict, &[], Mode::Legacy, false),
            Decision::Proceed
        );
    }

    #[test]
    fn skip_checking_vouches_for_bytes_not_for_sizes() {
        let same_size = Overlap {
            conflicts: vec![],
            unproven: vec!["/d/P/a".into()],
            with: vec!["x".into()],
        };
        assert_eq!(
            decide(&same_size, &[], Mode::Isolate, true),
            Decision::Proceed
        );
        assert_eq!(
            decide(&same_size, &[], Mode::Isolate, false),
            Decision::Isolate
        );
        let other_size = Overlap {
            conflicts: vec!["/d/P/a".into()],
            unproven: vec![],
            with: vec!["x".into()],
        };
        assert_eq!(
            decide(&other_size, &[], Mode::Isolate, true),
            Decision::Isolate
        );
    }

    #[test]
    fn hardlinked_files_nobody_claims_are_proven_before_a_download() {
        let clear = Overlap::default();
        assert_eq!(decide(&clear, &[], Mode::Isolate, false), Decision::Proceed);
        assert_eq!(
            decide(&clear, &["a.mkv".into()], Mode::Isolate, false),
            Decision::ProveInPlace(vec!["a.mkv".into()])
        );
        assert_eq!(
            decide(&clear, &["a.mkv".into()], Mode::Isolate, true),
            Decision::Proceed
        );
    }

    #[test]
    fn the_isolated_folder_says_which_release_it_is() {
        assert_eq!(
            isolated_save_path("/data/tv", "Show S02 GROUP", "0a1b2c3d4e5f6071"),
            PathBuf::from("/data/tv/Show S02 GROUP [0a1b2c3d]")
        );
        // A single file gets a folder named after it.
        assert_eq!(
            isolated_save_path("/data/movies", "Film.mkv", "9f8e7d6c5b"),
            PathBuf::from("/data/movies/Film.mkv [9f8e7d6c]")
        );
    }

    #[test]
    fn a_second_add_on_the_same_branch_waits_for_the_first() {
        let root = std::env::temp_dir().join(format!("pathclaim-reserve-{}", std::process::id()));
        let first = reserve(&root.join("Pack"));
        let t0 = Instant::now();
        let waiter = std::thread::spawn({
            let p = root.join("Pack").join("a.mkv");
            move || {
                let _r = reserve(&p);
                Instant::now()
            }
        });
        std::thread::sleep(Duration::from_millis(150));
        // An unrelated path does not wait.
        let _other = reserve(&root.join("Other"));
        assert!(t0.elapsed() < Duration::from_millis(1000));
        drop(first);
        let got = waiter.join().unwrap();
        assert!(
            got.duration_since(t0) >= Duration::from_millis(150),
            "the second add did not wait"
        );
    }

    /// A multi-file torrent with real SHA-1 pieces over `files`' contents.
    fn torrent_over(name: &str, files: &[(&str, &[u8])], piece_len: usize) -> Vec<u8> {
        use sha1::{Digest, Sha1};
        let stream: Vec<u8> = files.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        let mut pieces = Vec::new();
        for chunk in stream.chunks(piece_len) {
            pieces.extend_from_slice(&Sha1::digest(chunk));
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"d4:infod5:filesl");
        for (path, data) in files {
            v.extend_from_slice(
                format!("d6:lengthi{}e4:pathl{}:{path}ee", data.len(), path.len()).as_bytes(),
            );
        }
        v.extend_from_slice(
            format!(
                "e4:name{}:{name}12:piece lengthi{piece_len}e6:pieces{}:",
                name.len(),
                pieces.len()
            )
            .as_bytes(),
        );
        v.extend_from_slice(&pieces);
        v.extend_from_slice(b"ee");
        v
    }

    fn sha1_verify(bytes: &[u8]) -> impl Fn(u32, &[u8]) -> Option<bool> + '_ {
        move |p, d| {
            use sha1::{Digest, Sha1};
            let table = crate::dedup::pieces(bytes)?;
            let want = table.get(p as usize * 20..p as usize * 20 + 20)?;
            Some(Sha1::digest(d).as_slice() == want)
        }
    }

    #[test]
    fn a_same_size_impostor_is_not_proven_nor_the_file_sharing_its_piece() {
        // The .nfo case: the video is the same bytes, the .nfo has the same
        // length and other contents, and the two meet inside one piece.
        let dir = std::env::temp_dir().join(format!("pathclaim-prove-{}", std::process::id()));
        let src = dir.join("Pack");
        std::fs::create_dir_all(&src).unwrap();
        let video: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("a.mkv"), &video).unwrap();
        std::fs::write(src.join("a.nfo"), b"team NFO, version one").unwrap();

        let want = torrent_over(
            "Pack",
            &[("a.mkv", &video), ("a.nfo", b"team NFO, version two")],
            16384,
        );
        let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&want).unwrap();
        // Paths start at the save path (`dir`), folder included.
        let candidates: HashSet<PathBuf> = ["Pack/a.mkv", "Pack/a.nfo"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let proven = prove(&meta, &dir, &candidates, &sha1_verify(&want));

        // a.mkv's last piece also covers a.nfo, which fails: a.mkv stays
        // unproven too. Downloading costs bytes; linking a file whose last
        // piece did not verify would cost correctness.
        assert!(
            !proven.contains(&PathBuf::from("Pack/a.nfo")),
            "a same-size impostor was proven"
        );
        assert!(
            !proven.contains(&PathBuf::from("Pack/a.mkv")),
            "a file whose last piece failed was proven"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_whose_pieces_are_its_own_is_proven() {
        let dir = std::env::temp_dir().join(format!("pathclaim-own-{}", std::process::id()));
        let src = dir.join("Pack");
        std::fs::create_dir_all(&src).unwrap();
        let video: Vec<u8> = (0..32_768u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(src.join("a.mkv"), &video).unwrap();
        std::fs::write(src.join("b.nfo"), b"old notes 16 B..").unwrap();

        // a.mkv ends exactly on a piece boundary: its pieces are its own.
        let want = torrent_over(
            "Pack",
            &[("a.mkv", &video), ("b.nfo", b"new notes 16 B..")],
            16384,
        );
        let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&want).unwrap();
        let candidates: HashSet<PathBuf> = ["Pack/a.mkv", "Pack/b.nfo"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let proven = prove(&meta, &dir, &candidates, &sha1_verify(&want));
        assert!(proven.contains(&PathBuf::from("Pack/a.mkv")));
        assert!(!proven.contains(&PathBuf::from("Pack/b.nfo")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `torrent_over` with an extra key in the info dict, so two torrents of
    /// the same files get two info hashes (a tracker's `source` tag does
    /// exactly this).
    fn salted(name: &str, files: &[(&str, &[u8])], piece_len: usize, salt: &str) -> Vec<u8> {
        let mut v = torrent_over(name, files, piece_len);
        let tail = b"ee".len();
        let at = v.len() - tail;
        v.splice(
            at..at,
            format!("6:x-salt{}:{salt}", salt.len()).into_bytes(),
        );
        v
    }

    fn add(
        s: &crate::api::testing::TestState,
        bytes: &[u8],
        save: &Path,
        paused: bool,
        seed: bool,
    ) -> Result<String, String> {
        crate::api::add_torrent_bytes(
            &s.state,
            bytes,
            "",
            &save.to_string_lossy(),
            "",
            paused,
            seed,
            "hoard",
        )
        .map(|r| r.0)
    }

    fn engine_save_path(s: &crate::api::testing::TestState, hash: &str) -> PathBuf {
        let e = s.state.engines.get("hoard").expect("hoard engine");
        let t = e
            .manager
            .get(&crate::store::hex20(hash).unwrap())
            .expect("held");
        let p = t.save_path.read().clone();
        p
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_release_under_the_same_name_gets_its_own_folder() {
        // The production case: one folder name, one file name, two encodes.
        let s = crate::api::testing::state("pathclaim-isolate");
        let dir = s.dir.join("tv");
        std::fs::create_dir_all(dir.join("Pack")).unwrap();
        let first: Vec<u8> = (0..40_000u32).map(|i| (i % 241) as u8).collect();
        std::fs::write(dir.join("Pack").join("E01.mkv"), &first).unwrap();
        let a = add(
            &s,
            &torrent_over("Pack", &[("E01.mkv", &first)], 16384),
            &dir,
            true,
            true,
        )
        .unwrap();

        let second: Vec<u8> = (0..30_000u32).map(|i| (i % 239) as u8).collect();
        let b = add(
            &s,
            &torrent_over("Pack", &[("E01.mkv", &second)], 16384),
            &dir,
            false,
            false,
        )
        .unwrap();

        assert_eq!(engine_save_path(&s, &a), dir);
        assert_eq!(
            engine_save_path(&s, &b),
            dir.join(format!("Pack [{}]", &b[..8]))
        );
        assert_eq!(
            std::fs::read(dir.join("Pack").join("E01.mkv")).unwrap(),
            first,
            "the first release was touched"
        );
        assert!(recent()
            .iter()
            .any(|e| e.info_hash == b && e.action == "isolated"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_same_size_file_is_linked_only_once_proven() {
        // Same video, an .nfo of the same length with other words in it.
        let s = crate::api::testing::state("pathclaim-link");
        let dir = s.dir.join("movies");
        std::fs::create_dir_all(dir.join("Film")).unwrap();
        let video: Vec<u8> = (0..32_768u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("Film").join("a.mkv"), &video).unwrap();
        std::fs::write(dir.join("Film").join("a.nfo"), b"team notes v1...").unwrap();
        add(
            &s,
            &torrent_over(
                "Film",
                &[("a.mkv", &video), ("a.nfo", b"team notes v1...")],
                16384,
            ),
            &dir,
            true,
            true,
        )
        .unwrap();

        let b = add(
            &s,
            &torrent_over(
                "Film",
                &[("a.mkv", &video), ("a.nfo", b"uploader notes!!")],
                16384,
            ),
            &dir,
            true,
            false,
        )
        .unwrap();
        let root = dir.join(format!("Film [{}]", &b[..8])).join("Film");
        assert!(
            root.join("a.mkv").exists(),
            "the proven video was not linked: {:?}",
            recent()
                .iter()
                .filter(|e| e.info_hash == b)
                .collect::<Vec<_>>()
        );
        assert!(!root.join("a.nfo").exists(), "an unproven .nfo was linked");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let x = std::fs::metadata(dir.join("Film").join("a.mkv")).unwrap();
            let y = std::fs::metadata(root.join("a.mkv")).unwrap();
            assert_eq!(x.ino(), y.ino(), "linked, not copied");
        }
        assert_eq!(
            std::fs::read(dir.join("Film").join("a.nfo")).unwrap(),
            b"team notes v1..."
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuse_mode_refuses_and_says_why() {
        let s = crate::api::testing::state_from(
            "pathclaim-refuse",
            "[dedup]\npath_collision = \"refuse\"\n",
        );
        let dir = s.dir.join("d");
        std::fs::create_dir_all(dir.join("P")).unwrap();
        std::fs::write(dir.join("P").join("x.bin"), [1u8; 100]).unwrap();
        add(
            &s,
            &torrent_over("P", &[("x.bin", &[1u8; 100])], 16384),
            &dir,
            true,
            true,
        )
        .unwrap();
        let err = add(
            &s,
            &torrent_over("P", &[("x.bin", &[2u8; 120])], 16384),
            &dir,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("already another torrent's"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_bytes_from_another_tracker_share_the_path() {
        // Cross-seeding: identical pieces and files, another info hash.
        let s = crate::api::testing::state("pathclaim-xseed");
        let dir = s.dir.join("d");
        std::fs::create_dir_all(dir.join("P")).unwrap();
        let data = [7u8; 5000];
        std::fs::write(dir.join("P").join("x.bin"), data).unwrap();
        add(
            &s,
            &salted("P", &[("x.bin", &data)], 16384, "one"),
            &dir,
            true,
            true,
        )
        .unwrap();
        let b = add(
            &s,
            &salted("P", &[("x.bin", &data)], 16384, "two"),
            &dir,
            true,
            true,
        )
        .unwrap();
        assert_eq!(engine_save_path(&s, &b), dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recheck_never_fetches_over_a_file_another_torrent_holds() {
        // The state production is already in: two torrents sharing a path
        // with different bytes (legacy mode reproduces it). Verify on the
        // second must hold it, not fetch over the first one's file.
        let s = crate::api::testing::state_from(
            "pathclaim-guard",
            "[dedup]\npath_collision = \"legacy\"\n",
        );
        let dir = s.dir.join("d");
        std::fs::create_dir_all(dir.join("P")).unwrap();
        std::fs::write(dir.join("P").join("x.nfo"), b"the first notes!").unwrap();
        add(
            &s,
            &torrent_over("P", &[("x.nfo", b"the first notes!")], 16384),
            &dir,
            true,
            true,
        )
        .unwrap();
        let b = add(
            &s,
            &torrent_over("P", &[("x.nfo", b"other notes, 16B")], 16384),
            &dir,
            true,
            true,
        )
        .unwrap();
        assert_eq!(engine_save_path(&s, &b), dir, "legacy mode shares the path");

        let mgr = s.state.engines.get("hoard").unwrap().manager.clone();
        let ih = crate::store::hex20(&b).unwrap();
        mgr.start_torrent(&ih).unwrap();
        let t = mgr.get(&ih).unwrap();
        assert!(!t.is_paused.load(std::sync::atomic::Ordering::Relaxed));
        guarded_recheck(&s.state, "hoard", &t).unwrap();
        assert!(
            t.is_paused.load(std::sync::atomic::Ordering::Relaxed),
            "it was let loose"
        );
        assert_eq!(
            t.status.load(std::sync::atomic::Ordering::Relaxed),
            typhon_engine::torrent::meta::TorrentStatus::Error as u8,
            "held without saying why"
        );
        assert!(recent()
            .iter()
            .any(|e| e.info_hash == b && e.action == "held"));
        assert_eq!(
            std::fs::read(dir.join("P").join("x.nfo")).unwrap(),
            b"the first notes!"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_linked_from_the_library_is_proven_before_a_download() {
        // Nobody's torrent, but a second name in the *arr library: a
        // download over it would rewrite the library copy too.
        let s = crate::api::testing::state("pathclaim-foreign");
        let dir = s.dir.join("d");
        let lib = s.dir.join("library");
        std::fs::create_dir_all(dir.join("P")).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(dir.join("P").join("ep.mkv"), [3u8; 20_000]).unwrap();
        std::fs::hard_link(dir.join("P").join("ep.mkv"), lib.join("ep.mkv")).unwrap();

        // Other bytes at that path: held, nothing written.
        let other = add(
            &s,
            &torrent_over("P", &[("ep.mkv", &[4u8; 20_000])], 16384),
            &dir,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            engine_save_path(&s, &other),
            dir,
            "proven in place, not isolated"
        );
        let t = s
            .state
            .engines
            .get("hoard")
            .unwrap()
            .manager
            .get(&crate::store::hex20(&other).unwrap())
            .unwrap();
        assert!(
            t.is_paused.load(std::sync::atomic::Ordering::Relaxed),
            "asked to run, must stay held"
        );
        assert!(recent()
            .iter()
            .any(|e| e.info_hash == other && e.action == "held"));
        assert_eq!(
            std::fs::read(lib.join("ep.mkv")).unwrap(),
            vec![3u8; 20_000]
        );

        // Start, as after a restart forgot the proof: asked again, still held.
        crate::api::apply_pause_to_engine(&s.state, "hoard", &other, false);
        assert!(
            t.is_paused.load(std::sync::atomic::Ordering::Relaxed),
            "Start let it write over the library"
        );
        assert_eq!(
            std::fs::read(lib.join("ep.mkv")).unwrap(),
            vec![3u8; 20_000]
        );
    }

    /// A single-file torrent (`length` in the info dict, no `files`).
    fn single_over(name: &str, data: &[u8], salt: &str) -> Vec<u8> {
        use sha1::{Digest, Sha1};
        let mut pieces = Vec::new();
        for chunk in data.chunks(16384) {
            pieces.extend_from_slice(&Sha1::digest(chunk));
        }
        let mut v = Vec::new();
        v.extend_from_slice(
            format!(
                "d4:infod6:lengthi{}e4:name{}:{name}12:piece lengthi16384e6:pieces{}:",
                data.len(),
                name.len(),
                pieces.len()
            )
            .as_bytes(),
        );
        v.extend_from_slice(&pieces);
        v.extend_from_slice(format!("6:x-salt{}:{salt}ee", salt.len()).as_bytes());
        v
    }

    /// The store lookup must find exactly what walking every torrent finds,
    /// on the shapes that make it non-trivial: a torrent rooted above ours,
    /// one rooted inside it, a sibling under the same name, one two levels
    /// deep, an unrelated one, one whose store row spells its root rather than
    /// its save path (the Internet Archive layout) -- and after a move.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_store_lookup_finds_what_the_full_walk_finds() {
        let s = crate::api::testing::state_from(
            "pathclaim-index",
            "[dedup]\npath_collision = \"legacy\"\n",
        );
        let t = s.dir.join("t");
        let lib = t.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let fixtures: Vec<(Vec<u8>, PathBuf)> = vec![
            (
                torrent_over(
                    "Pack",
                    &[("a.mkv", &[1u8; 300]), ("sub/b.nfo", &[2u8; 30])],
                    16384,
                ),
                lib.clone(),
            ),
            (
                salted("lib", &[("Pack/a.mkv", &[1u8; 300])], 16384, "above"),
                t.clone(),
            ),
            (
                single_over("a.mkv", &[3u8; 400], "inside"),
                lib.join("Pack"),
            ),
            (
                single_over("b.nfo", &[4u8; 30], "deep"),
                lib.join("Pack").join("sub"),
            ),
            (
                salted("Pack", &[("c.txt", &[5u8; 10])], 16384, "sibling"),
                lib.clone(),
            ),
            (
                salted("Other", &[("a.mkv", &[6u8; 300])], 16384, "unrelated"),
                lib.clone(),
            ),
            (
                salted("lib", &[("Pack/sub/b.nfo", &[7u8; 30])], 16384, "ia"),
                t.clone(),
            ),
        ];
        let mut hashes = Vec::new();
        for (bytes, save) in &fixtures {
            hashes.push(add(&s, bytes, save, true, true).unwrap());
        }
        // The Internet Archive layout: the store row names the item's own
        // folder, the engine its parent.
        {
            let store = s.state.store.lock().unwrap();
            store
                .set_save_path(&hashes[6], &t.join("lib").to_string_lossy())
                .unwrap();
        }
        let mgr = s.state.engines.get("hoard").unwrap().manager.clone();

        let compare = |label: &str| {
            for h in &hashes {
                let me = mgr.get(&crate::store::hex20(h).unwrap()).unwrap();
                let save = me.save_path.read().clone();
                let mine: HashSet<PathBuf> = files_of(&me.meta, &save)
                    .into_iter()
                    .map(|(a, _, _)| path_key(&a))
                    .collect();
                let top = if me.meta.multi_file {
                    Path::new(&me.meta.name)
                        .components()
                        .next()
                        .map(|c| c.as_os_str().to_os_string())
                } else {
                    me.meta.files[0]
                        .path
                        .components()
                        .next()
                        .map(|c| c.as_os_str().to_os_string())
                };
                let ih = me.meta.info_hash;
                let skip = move |o: &TorrentState| o.meta.info_hash == ih;
                let norm = |v: Vec<(Arc<TorrentState>, Vec<(PathBuf, u64)>)>| {
                    let mut v: Vec<(String, Vec<(PathBuf, u64)>)> = v
                        .into_iter()
                        .map(|(t, mut f)| {
                            f.sort();
                            (typhon_engine::torrent::hex_encode(&t.meta.info_hash), f)
                        })
                        .collect();
                    v.sort();
                    v
                };
                let fast = norm(crate::jobsrun::scan_claims(
                    &s.state,
                    &save,
                    top.as_deref(),
                    &mine,
                    &skip,
                ));
                let full = norm(crate::jobsrun::scan_claims_full(
                    &s.state,
                    &save,
                    top.as_deref(),
                    &mine,
                    &skip,
                ));
                assert_eq!(
                    fast,
                    full,
                    "{label}: lookup and walk disagree for {}",
                    &h[..8]
                );
            }
        };
        compare("as added");
        // The case the test exists for must be in the data: above, inside and
        // deep each share a file with the pack.
        let pack = mgr.get(&crate::store::hex20(&hashes[0]).unwrap()).unwrap();
        let mine: HashSet<PathBuf> = files_of(&pack.meta, &lib)
            .into_iter()
            .map(|(a, _, _)| path_key(&a))
            .collect();
        let ih = pack.meta.info_hash;
        let found = crate::jobsrun::scan_claims(
            &s.state,
            &lib,
            Some(std::ffi::OsStr::new("Pack")),
            &mine,
            &move |o: &TorrentState| o.meta.info_hash == ih,
        );
        assert_eq!(
            found.len(),
            4,
            "above, inside, deep, and the item whose store row spells its root"
        );

        // Moved, as a move does it: the engine and the store row together.
        let moved = &hashes[5];
        mgr.set_save_path(&crate::store::hex20(moved).unwrap(), &t.to_string_lossy())
            .unwrap();
        s.state
            .store
            .lock()
            .unwrap()
            .set_save_path(moved, &t.to_string_lossy())
            .unwrap();
        compare("after a move");
    }

    #[test]
    fn a_missing_source_proves_nothing() {
        let dir = std::env::temp_dir().join(format!("pathclaim-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let want = torrent_over("Pack", &[("a.mkv", &[1u8; 100])], 16384);
        let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&want).unwrap();
        let candidates: HashSet<PathBuf> = [PathBuf::from("Pack/a.mkv")].into_iter().collect();
        assert!(prove(&meta, &dir, &candidates, &sha1_verify(&want)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
