//! Which folders the daemon already knows, for the routes that READ a path a
//! caller names.
//!
//! A path given by a client is not read wherever the process happens to be
//! allowed to: it has to lie under a folder the daemon itself already works
//! with. Anything that reads or lists a caller-supplied path (the MCP
//! `.torrent` path, a folder listing) goes through one of two checks:
//!
//! - [`within_known_dirs`]: every folder the daemon works with, its own data
//!   folder included;
//! - [`within_download_dirs`]: the same, WITHOUT the data folder, and refusing
//!   anything inside it: the config and the database (and their names) are
//!   never reachable through it. For a listing a browser shows.
//!
//! The MCP `.torrent` path is narrower than both: [`within_torrent_roots`],
//! the folders of `[mcp] torrent_roots` and nothing else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The answer for every refusal, whatever the reason (relative, missing, not a
/// folder the daemon knows): the caller learns nothing about the disk.
pub(crate) const NOT_KNOWN: &str = "the path is not under a folder this node knows";

/// `path`, canonicalised, if it lies under a folder the daemon already knows;
/// an error that says nothing about why otherwise.
///
/// The known folders are the data folder (`[daemon] data_dir`), each
/// category's save path (and graduation folder), the default save path a
/// client set, and the save path of every torrent any engine holds. The path
/// must be absolute. Symbolic links and `..` are resolved BEFORE the
/// comparison (the folders are resolved too), so a link inside a known folder
/// that points out of it is refused, and one that points to another known
/// folder is accepted. A path that does not exist is refused: there is
/// nothing to resolve.
///
/// A folder that resolves to `/` is never a root. Neither is a TORRENT's save
/// path with fewer than two components (`/mnt`, `/tmp`): any caller of the API
/// can add a torrent with the save path of its choice, so these would make a
/// whole disk "known". Deeper save paths are kept (`/mnt/user/films`), and
/// the configured folders (data folder, categories, default save path) keep
/// working at any depth above `/`, since a one-folder install (`/downloads`,
/// `/config`) is normal.
///
/// Blocking (it stats the disk): call it from a blocking thread.
#[cfg(test)]
pub(crate) fn within_known_dirs(
    state: &crate::api::AppState,
    path: &str,
) -> Result<PathBuf, String> {
    check(state, path, true)
}

/// [`within_known_dirs`] without the data folder: for a listing the web UI
/// shows. The data folder holds the config (API key, password hash) and the
/// database, so a path inside it is refused outright, even where a category or
/// a torrent's save path would cover it, and it is not a root.
pub(crate) fn within_download_dirs(
    state: &crate::api::AppState,
    path: &str,
) -> Result<PathBuf, String> {
    check(state, path, false)
}

/// `path`, canonicalised, if it lies under a folder of `[mcp] torrent_roots`;
/// a refusal that says nothing about why otherwise. No list (the default) = no
/// path is read.
///
/// Narrower than [`within_known_dirs`] on purpose: every folder THAT one trusts
/// (a torrent's save path, the default save path, a category's folder, and
/// `[daemon] data_dir`, which `/api/settings` writes and a restart then reads)
/// can be set by a caller of the API, so a caller could point one at another
/// program's folder and then read from it. `torrent_roots` is set in the
/// config file only. Same rules for the path: absolute, links and `..`
/// resolved first, `/` never a root.
pub(crate) fn within_torrent_roots(
    state: &crate::api::AppState,
    path: &str,
) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(NOT_KNOWN.to_string());
    }
    let canon = std::fs::canonicalize(p).map_err(|_| NOT_KNOWN.to_string())?;
    if lies_under(&canon, &state.boot_roots.torrent_roots) {
        return Ok(canon);
    }
    Err(NOT_KNOWN.to_string())
}

/// The folders the daemon trusts, read ONCE, when it starts, from the config
/// file.
///
/// `data_dir` is only for [`within_known_dirs`] and [`within_download_dirs`];
/// the `.torrent` read does not trust it (`/api/settings` can write it, and the
/// next start reads it). `torrent_roots` is never written by a route at all,
/// and is the only folder list that read trusts.
#[derive(Debug, Clone, Default)]
pub(crate) struct BootRoots {
    pub(crate) data_dir: PathBuf,
    pub(crate) torrent_roots: Vec<PathBuf>,
}

impl BootRoots {
    pub(crate) fn from_config(cfg: &crate::config::Config) -> Self {
        BootRoots {
            data_dir: PathBuf::from(&cfg.daemon.data_dir),
            torrent_roots: cfg.mcp.torrent_roots.iter().map(PathBuf::from).collect(),
        }
    }
}

fn check(state: &crate::api::AppState, path: &str, with_data_dir: bool) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(NOT_KNOWN.to_string());
    }
    let canon = std::fs::canonicalize(p).map_err(|_| NOT_KNOWN.to_string())?;

    let data = state.boot_roots.data_dir.clone();
    if !with_data_dir && lies_under(&canon, std::slice::from_ref(&data)) {
        return Err(NOT_KNOWN.to_string());
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if with_data_dir {
        roots.push(data);
    }
    roots.extend(
        crate::api::category_save_paths(state)
            .into_iter()
            .map(PathBuf::from),
    );
    roots.push(PathBuf::from(crate::api::qbit_default_save_path(state)));
    if lies_under(&canon, &roots) {
        return Ok(canon);
    }
    if torrent_dirs(state).iter().any(|d| canon.starts_with(d)) {
        return Ok(canon);
    }
    Err(NOT_KNOWN.to_string())
}

/// Whether the canonical `path` lies under any of `roots` (each resolved the
/// same way; an empty, missing or `/` root is no root).
fn lies_under(path: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .filter(|r| !r.as_os_str().is_empty())
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .filter(|r| r.parent().is_some())
        .any(|r| path.starts_with(&r))
}

/// The canonical save paths of every torrent, as roots (see
/// [`within_known_dirs`] for which are dropped).
///
/// Each engine keeps the set of its save paths as torrents come, go and move
/// (`TorrentManager::save_paths`): a few dozen keys at a million torrents, read
/// under a lock with no walk of the catalogue, so a refusal costs the same
/// whatever the library holds and however fast torrents are being added. Each
/// key is canonicalised through [`canonical`].
fn torrent_dirs(state: &crate::api::AppState) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = state
        .engines
        .engines()
        .iter()
        .flat_map(|e| e.manager.save_paths())
        .filter_map(|p| canonical(&p))
        // Two components below `/` at least: `/` is the root itself, one
        // component is a whole mount.
        .filter(|p| p.components().count() >= 3)
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// `std::fs::canonicalize` with a small memory of the answers, so the few
/// dozen save paths are not resolved again on every call. Only successes are
/// kept (a folder created later is found), and the memory is emptied when it
/// reaches [`CANONICAL_CAP`]. A link retargeted after it was remembered keeps
/// its old answer until then: the comparison stays against where the folder
/// WAS resolved, never against somewhere wider.
fn canonical(path: &Path) -> Option<PathBuf> {
    const CANONICAL_CAP: usize = 4096;
    static MEMORY: Mutex<Option<HashMap<PathBuf, PathBuf>>> = Mutex::new(None);
    let mut slot = MEMORY.lock().unwrap_or_else(|p| p.into_inner());
    let memory = slot.get_or_insert_with(HashMap::new);
    if let Some(c) = memory.get(path) {
        return Some(c.clone());
    }
    let c = std::fs::canonicalize(path).ok()?;
    if memory.len() >= CANONICAL_CAP {
        memory.clear();
    }
    memory.insert(path.to_path_buf(), c.clone());
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::{state_from, TestState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str, extra: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n{extra}"))
    }

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xCDu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// A torrent saved at `save_path`, held paused and trusted as complete so
    /// nothing is written there. Its info hash.
    fn add_at(s: &TestState, name: &str, save_path: &str) -> String {
        crate::api::add_torrent_bytes(
            &s.state,
            &torrent_bytes(name),
            "",
            save_path,
            "",
            true,
            true,
            "hoard",
        )
        .unwrap_or_else(|e| panic!("add {name}: {e}"))
        .0
    }

    /// The manager and info hash of a torrent the daemon holds.
    fn manager_and_hash(
        s: &TestState,
        hash: &str,
    ) -> (
        std::sync::Arc<typhon_engine::torrent::TorrentManager>,
        typhon_engine::torrent::meta::InfoHash,
    ) {
        let (engine, t) = crate::api::find_torrent(&s.state, hash).expect("held");
        (
            s.state.engines.get(&engine).unwrap().manager.clone(),
            t.meta.info_hash,
        )
    }

    fn file_in(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let f = dir.join(name);
        std::fs::write(&f, b"x").unwrap();
        f
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pathroots-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_path_lies_under_a_root_by_component_not_by_prefix() {
        let d = scratch("lies");
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::create_dir_all(d.join("data-other")).unwrap();
        let root = std::fs::canonicalize(d.join("data")).unwrap();
        let other = std::fs::canonicalize(d.join("data-other")).unwrap();
        assert!(lies_under(&root.join("x"), std::slice::from_ref(&root)));
        assert!(lies_under(&root, std::slice::from_ref(&root)));
        assert!(!lies_under(&other.join("x"), std::slice::from_ref(&root)));
        assert!(!lies_under(&root, &[PathBuf::new()]));
        assert!(!lies_under(&root, &[d.join("missing")]));
        assert!(!lies_under(&root, &[PathBuf::from("/")]), "/ is no root");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A torrent added with `/` as its save path does not make the disk known.
    #[test]
    fn a_torrent_saved_at_the_filesystem_root_makes_nothing_known() {
        let s = st("pr-slash", "");
        add_at(&s, "rooted", "/");
        let out = scratch("slash");
        let f = file_in(&out, "a.torrent");
        assert!(within_known_dirs(&s.state, &f.to_string_lossy()).is_err());
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Nor does one saved a single folder below it: that is a whole mount.
    #[test]
    fn a_torrent_saved_one_folder_below_the_root_makes_nothing_known() {
        let s = st("pr-one", "");
        add_at(&s, "shallow", "/tmp");
        let out = scratch("one");
        let f = file_in(&out, "a.torrent");
        assert!(within_known_dirs(&s.state, &f.to_string_lossy()).is_err());
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The data folder is known to `within_known_dirs` and invisible to
    /// `within_download_dirs`.
    #[test]
    fn the_data_folder_is_known_but_not_a_download_folder() {
        let data = scratch("data");
        let s = st("pr-data", &format!("data_dir = \"{}\"\n", data.display()));
        let f = file_in(&data, "default.toml");
        assert!(within_known_dirs(&s.state, &f.to_string_lossy()).is_ok());
        assert!(within_download_dirs(&s.state, &f.to_string_lossy()).is_err());
        // Even a torrent saved inside it does not open it to a listing.
        let deep = data.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        add_at(&s, "inside", &deep.to_string_lossy());
        let g = file_in(&deep, "c.torrent");
        assert!(within_known_dirs(&s.state, &g.to_string_lossy()).is_ok());
        assert!(within_download_dirs(&s.state, &g.to_string_lossy()).is_err());
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A folder a torrent is saved in is known from the next call; once its
    /// last torrent is gone it is not; a move makes the old folder unknown and
    /// the new one known.
    #[test]
    fn the_folders_follow_the_torrents_as_they_come_go_and_move() {
        let s = st("pr-follow", "");
        let base = scratch("follow");
        let (one, two, other) = (
            base.join("lib").join("one"),
            base.join("lib").join("two"),
            base.join("lib").join("other"),
        );
        let (f1, f2, fo) = (
            file_in(&one, "a.torrent"),
            file_in(&two, "a.torrent"),
            file_in(&other, "a.torrent"),
        );
        let known = |f: &PathBuf| within_known_dirs(&s.state, &f.to_string_lossy()).is_ok();
        assert!(!known(&f1) && !known(&f2));

        let h1 = add_at(&s, "first", &one.to_string_lossy());
        assert!(known(&f1), "known at once");
        // A second torrent in the same folder keeps it known after the first goes.
        let h1b = add_at(&s, "second", &one.to_string_lossy());
        let (m, ih) = manager_and_hash(&s, &h1);
        m.remove_torrent(&ih, true).unwrap();
        assert!(known(&f1), "one torrent still saved there");
        let (m, ihb) = manager_and_hash(&s, &h1b);
        m.set_save_path(&ihb, &two.to_string_lossy()).unwrap();
        assert!(!known(&f1), "the old folder is unknown after the move");
        assert!(known(&f2), "the new folder is known after the move");
        m.remove_torrent(&ihb, true).unwrap();
        assert!(!known(&f2), "the last torrent is gone");
        assert!(!known(&fo));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A refusal costs the same after an add as before it: nothing is walked.
    /// (A rebuild of the folder set at this size takes tens of milliseconds
    /// even on a fast machine; a lookup takes microseconds.)
    #[test]
    fn a_refusal_right_after_an_add_does_not_walk_the_catalogue() {
        use typhon_engine::torrent::meta::TorrentState;
        let s = st("pr-fast", "");
        let engines = s.state.engines.engines();
        let manager = engines[0].manager.clone();
        let base =
            typhon_engine::torrent::metainfo::parse_torrent_bytes(&torrent_bytes("bench")).unwrap();
        let root = scratch("fast");
        let dirs: Vec<PathBuf> = (0..20)
            .map(|i| root.join(format!("vol{i}")).join("lib"))
            .collect();
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
        }
        let mut n = 0u64;
        let mut add_one = |manager: &typhon_engine::torrent::TorrentManager| {
            let mut meta = base.clone();
            meta.info_hash[..8].copy_from_slice(&n.to_be_bytes());
            let st = TorrentState::new(meta, dirs[n as usize % dirs.len()].clone(), true);
            manager.insert_prebuilt(std::sync::Arc::new(st));
            n += 1;
        };
        for _ in 0..150_000 {
            add_one(&manager);
        }
        let outside = file_in(&root.join("elsewhere").join("deep"), "a.torrent");
        let path = outside.to_string_lossy().into_owned();
        assert!(within_known_dirs(&s.state, &path).is_err());
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..20 {
            add_one(&manager);
            let t = std::time::Instant::now();
            assert!(within_known_dirs(&s.state, &path).is_err());
            worst = worst.max(t.elapsed());
        }
        assert!(
            worst < std::time::Duration::from_millis(20),
            "a refusal after an add took {worst:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn rss_kb() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|t| {
                t.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .map(String::from)
            })
            .and_then(|l| l.split_whitespace().nth(1).and_then(|n| n.parse().ok()))
            .unwrap_or(0)
    }

    /// At the size of the production catalogue: what a refused call costs, and
    /// what an insertion costs with the save path counts kept. Run apart (it
    /// holds a million torrents):
    /// `cargo test --release --bin hydranos pathroots::tests::cost_at -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn cost_at_a_million_torrents() {
        use typhon_engine::torrent::meta::TorrentState;
        let s = st("pr-million", "");
        let engines = s.state.engines.engines();
        let manager = engines[0].manager.clone();
        let base =
            typhon_engine::torrent::metainfo::parse_torrent_bytes(&torrent_bytes("bench")).unwrap();
        let root = scratch("million");
        let dirs: Vec<PathBuf> = (0..40)
            .map(|i| root.join(format!("vol{i}")).join("lib"))
            .collect();
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
        }
        let outside = file_in(&root.join("elsewhere").join("deep"), "a.torrent");
        let path = outside.to_string_lossy().into_owned();
        let states = |from: usize, to: usize| -> Vec<std::sync::Arc<TorrentState>> {
            (from..to)
                .map(|i| {
                    let mut meta = base.clone();
                    meta.info_hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
                    std::sync::Arc::new(TorrentState::new(meta, dirs[i % dirs.len()].clone(), true))
                })
                .collect()
        };
        let mut filled = 0usize;
        for target in [100_000usize, 300_000, 1_100_000] {
            let rss = rss_kb();
            if filled > 0 && rss / filled as u64 * target as u64 > 20 * 1024 * 1024 {
                eprintln!(
                    "stopping before {target}: projected RSS above 20 GB (now {} MB at {filled})",
                    rss / 1024
                );
                break;
            }
            let batch = states(filled, target);
            let t0 = std::time::Instant::now();
            for st in batch {
                manager.insert_prebuilt(st);
            }
            let insert_total = t0.elapsed();
            eprintln!(
                "{target} torrents in memory: {} insertions in {insert_total:?} = {:?} each (RSS {} MB)",
                target - filled,
                insert_total / (target - filled) as u32,
                rss_kb() / 1024
            );
            filled = target;
            let _ = within_known_dirs(&s.state, &path);
            let n = 10_000u32;
            let t1 = std::time::Instant::now();
            for _ in 0..n {
                assert!(within_known_dirs(&s.state, &path).is_err());
            }
            eprintln!(
                "refused call at {filled} torrents: {:?} each (mean of {n})",
                t1.elapsed() / n
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
