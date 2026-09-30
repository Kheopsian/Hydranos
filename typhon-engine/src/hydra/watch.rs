//! Watched folders: drop a `.torrent` (or a `.magnet`) in, it is added.
//!
//! - **Polled, not inotify.** inotify sees nothing written through SMB, NFS
//!   or Unraid's `/mnt/user` FUSE layer -- exactly where people point a watch
//!   folder -- and a watcher that silently misses files is worse than a scan
//!   every ten seconds.
//! - **A file is read once it has stopped changing**: same size and mtime on
//!   two scans in a row. A `.torrent` still being copied in would otherwise be
//!   read half-written and moved aside as invalid.
//! - **The add is the add**: `api::add_torrent_bytes`, or `magnets::request`
//!   for a `.magnet`, with the folder's category -- which already decides the
//!   engine and the save path, so a watch folder needs no routing of its own.
//! - **Nothing is deleted.** An added file goes to `added/`, a refused one is
//!   renamed `.invalid` with a `.txt` beside it saying why. A category typed
//!   wrong is then a mistake one can see and undo.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::AppState;

const SETTINGS_KEY: &str = "watch_folders";
/// Between two scans.
const SCAN_EVERY: Duration = Duration::from_secs(10);
/// A `.torrent` bigger than this is not one.
const MAX_FILE: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Folder {
    pub path: String,
    #[serde(default)]
    pub category: String,
    /// Force an engine instead of the category's. Empty: the category decides.
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FolderStatus {
    pub last_scan: i64,
    pub added: u64,
    pub refused: u64,
    /// Why the folder itself could not be read, if it could not.
    pub error: String,
    pub last_refusal: String,
}

/// What each file looked like at the previous scan, per folder: (size, mtime).
/// Per folder so a scan only ever replaces what it looked at.
type Seen = HashMap<PathBuf, (u64, SystemTime)>;
static SEEN: Mutex<Option<HashMap<String, Seen>>> = Mutex::new(None);
static STATUS: Mutex<Option<HashMap<String, FolderStatus>>> = Mutex::new(None);

pub fn folders(state: &AppState) -> Vec<Folder> {
    state
        .store
        .lock()
        .ok()
        .and_then(|s| s.setting(SETTINGS_KEY).ok().flatten())
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// A free name in `dir` for `name`: `name`, then `name (2)`, `name (3)`...
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// What became of one file.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Added,
    /// Already here: moved to `added/` like an add -- it IS in the client.
    AlreadyThere,
    Refused(String),
}

/// Add one file, then move it where its outcome says.
pub fn take(state: &AppState, f: &Folder, file: &Path) -> Outcome {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let is_magnet = name.to_ascii_lowercase().ends_with(".magnet");
    let result = match std::fs::read(file) {
        Err(e) => Err(format!("cannot read: {e}")),
        Ok(bytes) if is_magnet => {
            let text = String::from_utf8_lossy(&bytes);
            let link = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
            crate::magnets::request(state, &link, &f.category, "", "", f.paused, &f.engine).map(|_| ())
        }
        Ok(bytes) => {
            crate::api::add_torrent_bytes(state, &bytes, &f.category, "", "", f.paused, false, &f.engine).map(|_| ())
        }
    };
    let outcome = match result {
        Ok(()) => Outcome::Added,
        Err(e) if e.contains("already added") => Outcome::AlreadyThere,
        Err(e) => Outcome::Refused(e),
    };
    let dir = file.parent().unwrap_or(Path::new("."));
    match &outcome {
        Outcome::Added | Outcome::AlreadyThere => {
            let done = dir.join("added");
            let _ = std::fs::create_dir_all(&done);
            if let Err(e) = std::fs::rename(file, free_name(&done, &name)) {
                tracing::warn!(file = %file.display(), error = %e, "watch: added, but could not move the file aside");
            }
        }
        Outcome::Refused(why) => {
            let bad = free_name(dir, &format!("{name}.invalid"));
            let _ = std::fs::rename(file, &bad);
            let _ = std::fs::write(PathBuf::from(format!("{}.txt", bad.display())), format!("{why}\n"));
        }
    }
    outcome
}

/// One pass over every enabled folder.
pub fn scan(state: &AppState) {
    let now = crate::store::now_secs();
    for f in folders(state).into_iter().filter(|f| f.enabled) {
        let seen: Seen = SEEN
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|m| m.get(&f.path).cloned()))
            .unwrap_or_default();
        let mut seen_next: Seen = HashMap::new();
        let mut st = STATUS
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|m| m.get(&f.path).cloned()))
            .unwrap_or_default();
        st.last_scan = now;
        scan_folder(state, &f, &seen, &mut seen_next, &mut st);
        if let Ok(mut g) = SEEN.lock() {
            g.get_or_insert_with(HashMap::new).insert(f.path.clone(), seen_next);
        }
        if let Ok(mut g) = STATUS.lock() {
            g.get_or_insert_with(HashMap::new).insert(f.path.clone(), st);
        }
    }
}

fn scan_folder(state: &AppState, f: &Folder, seen: &Seen, seen_next: &mut Seen, st: &mut FolderStatus) {
    let entries = match std::fs::read_dir(&f.path) {
        Ok(e) => {
            st.error.clear();
            e
        }
        Err(e) => {
            st.error = format!("cannot read the folder: {e}");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let lower = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if !(lower.ends_with(".torrent") || lower.ends_with(".magnet")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let sig = (meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        // New, or changed since the last scan: still being written.
        if seen.get(&path) != Some(&sig) {
            seen_next.insert(path, sig);
            continue;
        }
        if meta.len() > MAX_FILE {
            let _ = take_refused(&path, "larger than 64 MB: not a .torrent");
            st.refused += 1;
            continue;
        }
        match take(state, f, &path) {
            Outcome::Added | Outcome::AlreadyThere => st.added += 1,
            Outcome::Refused(why) => {
                tracing::warn!(file = %path.display(), reason = %why, "watch: refused");
                st.refused += 1;
                st.last_refusal = why;
            }
        }
    }
}

fn take_refused(file: &Path, why: &str) -> std::io::Result<()> {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let bad = free_name(file.parent().unwrap_or(Path::new(".")), &format!("{name}.invalid"));
    std::fs::rename(file, &bad)?;
    std::fs::write(PathBuf::from(format!("{}.txt", bad.display())), format!("{why}\n"))
}

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SCAN_EVERY).await;
            let st = state.clone();
            let _ = tokio::task::spawn_blocking(move || scan(&st)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn unauthorised() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Invalid or missing API key"}))).into_response()
}

fn listing(state: &AppState) -> serde_json::Value {
    let status = STATUS.lock().ok().and_then(|g| g.clone()).unwrap_or_default();
    let rows: Vec<serde_json::Value> = folders(state)
        .into_iter()
        .map(|f| {
            let st = status.get(&f.path).cloned().unwrap_or_default();
            serde_json::json!({"folder": f, "status": st})
        })
        .collect();
    serde_json::json!({"folders": rows, "scan_every_secs": SCAN_EVERY.as_secs()})
}

async fn get_route(State(state): State<AppState>, RawQuery(q): RawQuery, headers: HeaderMap) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    Json(listing(&state)).into_response()
}

/// Replace the whole list. Each folder is checked now -- an absolute path,
/// a directory that exists, a category that exists -- so a typo is told at
/// once rather than found as a folder that never picks anything up.
async fn put_route(State(state): State<AppState>, RawQuery(q): RawQuery, headers: HeaderMap, body: String) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    let bad = |m: String| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": m}))).into_response();
    let list: Vec<Folder> = match serde_json::from_str(&body) {
        Ok(l) => l,
        Err(e) => return bad(e.to_string()),
    };
    let cats = crate::api::category_names(&state);
    for f in &list {
        if !f.path.starts_with('/') && !(cfg!(windows) && f.path.get(1..3) == Some(":\\")) {
            return bad(format!("{:?}: the folder must be an absolute path", f.path));
        }
        if !Path::new(&f.path).is_dir() {
            return bad(format!("{:?} is not a folder this node can see", f.path));
        }
        if f.category.is_empty() || !cats.iter().any(|c| *c == f.category) {
            return bad(format!("{:?}: pick an existing category -- it decides the engine and the save path", f.category));
        }
        if !f.engine.is_empty() && state.engines.get(&f.engine).is_none() {
            return bad(format!("no engine {:?}", f.engine));
        }
    }
    let json = serde_json::to_string(&list).unwrap_or_default();
    if let Err(e) = state.store.lock().map_err(|_| "store lock".to_string()).and_then(|s| s.put_setting(SETTINGS_KEY, &json).map_err(|e| e.to_string())) {
        return bad(e);
    }
    Json(listing(&state)).into_response()
}

pub fn routes() -> axum::Router<AppState> {
    use axum::routing::get;
    axum::Router::new().route("/api/watch", get(get_route).put(put_route))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::{body_json, keyed, state_from, TestState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn torrent(name: &str) -> Vec<u8> {
        let mut info = format!("d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:", name.len()).into_bytes();
        let mut piece = [0xEEu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let mut out = b"d4:info".to_vec();
        out.extend(info);
        out.push(b'e');
        out
    }

    /// A category of the test's own, so a folder has somewhere to send data.
    fn with_category(s: &TestState, name: &str) -> PathBuf {
        let data = s.dir.join(format!("data-{name}"));
        std::fs::create_dir_all(&data).unwrap();
        crate::api::testing::put_category(&s.state, name, "hoard", &data.to_string_lossy());
        data
    }

    fn folder(s: &TestState, name: &str, category: &str) -> PathBuf {
        let dir = s.dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let list = vec![Folder { path: dir.to_string_lossy().into(), category: category.into(), engine: String::new(), paused: true, enabled: true }];
        s.state.store.lock().unwrap().put_setting(SETTINGS_KEY, &serde_json::to_string(&list).unwrap()).unwrap();
        dir
    }

    /// ⭐ Dropped in, seen twice unchanged, added in the folder's category,
    /// moved to `added/`. A file seen once is left alone: it may still be
    /// being written.
    #[test]
    fn a_torrent_dropped_in_is_added_once_it_stops_changing() {
        let s = st("watch-add");
        with_category(&s, "books");
        let dir = folder(&s, "in", "books");
        std::fs::write(dir.join("one.torrent"), torrent("one")).unwrap();

        scan(&s.state);
        assert!(dir.join("one.torrent").exists(), "seen once: not touched yet");
        scan(&s.state);
        assert!(!dir.join("one.torrent").exists());
        assert!(dir.join("added/one.torrent").exists(), "moved aside, not deleted");
        let hash = typhon_engine::torrent::hex_encode(
            &typhon_engine::torrent::metainfo::parse_torrent_bytes(&torrent("one")).unwrap().info_hash,
        );
        let (engine, t) = crate::api::find_torrent(&s.state, &hash).expect("added");
        assert_eq!(engine, "hoard", "the category's engine");
        assert!(t.is_paused.load(std::sync::atomic::Ordering::Relaxed), "paused, as the folder says");
    }

    /// A file still growing between two scans is not read.
    #[test]
    fn a_file_still_being_written_waits() {
        let s = st("watch-growing");
        with_category(&s, "books");
        let dir = folder(&s, "in", "books");
        let f = dir.join("slow.torrent");
        let full = torrent("slow");
        std::fs::write(&f, &full[..10]).unwrap();
        scan(&s.state);
        std::fs::write(&f, &full[..30]).unwrap();
        scan(&s.state);
        assert!(f.exists(), "it changed between the scans: not yet");
        std::fs::write(&f, &full).unwrap();
        scan(&s.state);
        scan(&s.state);
        assert!(dir.join("added/slow.torrent").exists());
    }

    /// A refusal is kept, renamed, and says why beside itself; the same
    /// torrent dropped again goes to `added/` as already there.
    #[test]
    fn a_refused_file_says_why_and_a_duplicate_counts_as_added() {
        let s = st("watch-refuse");
        with_category(&s, "books");
        let dir = folder(&s, "in", "books");
        std::fs::write(dir.join("broken.torrent"), b"not bencode").unwrap();
        std::fs::write(dir.join("a.torrent"), torrent("dup")).unwrap();
        scan(&s.state);
        scan(&s.state);
        assert!(dir.join("broken.torrent.invalid").exists());
        let why = std::fs::read_to_string(dir.join("broken.torrent.invalid.txt")).unwrap();
        assert!(why.contains("did not parse"), "{why}");
        std::fs::write(dir.join("b.torrent"), torrent("dup")).unwrap();
        scan(&s.state);
        scan(&s.state);
        assert!(dir.join("added/b.torrent").exists(), "already in the client: filed as added");
        let st = STATUS.lock().unwrap().clone().unwrap();
        let fs = st.get(&dir.to_string_lossy().to_string()).unwrap();
        assert_eq!((fs.added, fs.refused), (2, 1));
    }

    /// A `.magnet` file holds a link; it becomes a magnet request.
    #[test]
    fn a_magnet_file_becomes_a_magnet_request() {
        let s = st("watch-magnet");
        with_category(&s, "books");
        let dir = folder(&s, "in", "books");
        let link = "magnet:?xt=urn:btih:c9e15763f722f23e98a29decdfae341b98d53056&dn=watched";
        std::fs::write(dir.join("x.magnet"), format!("{link}\n")).unwrap();
        scan(&s.state);
        scan(&s.state);
        assert!(dir.join("added/x.magnet").exists());
        let m = s.state.store.lock().unwrap().magnet("c9e15763f722f23e98a29decdfae341b98d53056").unwrap();
        assert_eq!(m.map(|m| m.category), Some("books".into()));
    }

    /// Names never overwrite: a second `x.torrent` in `added/` is `x (2).torrent`.
    #[test]
    fn a_moved_file_never_overwrites_another() {
        let d = std::env::temp_dir().join(format!("watch-free-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("x.torrent"), b"1").unwrap();
        assert_eq!(free_name(&d, "x.torrent"), d.join("x (2).torrent"));
        assert_eq!(free_name(&d, "y.torrent"), d.join("y.torrent"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The settings are checked when saved, not discovered later.
    #[tokio::test]
    async fn a_folder_is_checked_when_it_is_saved() {
        let s = st("watch-put");
        with_category(&s, "books");
        let dir = s.dir.join("real");
        std::fs::create_dir_all(&dir).unwrap();
        let put = |body: serde_json::Value| put_route(State(s.state.clone()), RawQuery(None), keyed(KEY), body.to_string());
        let r = put(serde_json::json!([{"path": "relative", "category": "books"}])).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = put(serde_json::json!([{"path": "/no/such/folder", "category": "books"}])).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = put(serde_json::json!([{"path": dir, "category": "nope"}])).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = put(serde_json::json!([{"path": dir, "category": "books"}])).await;
        assert!(r.status().is_success());
        let v = body_json(r).await;
        assert_eq!(v["folders"][0]["folder"]["enabled"], true, "enabled unless said otherwise");
    }
}
