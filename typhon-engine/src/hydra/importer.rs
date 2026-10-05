//! Taking a library over from another client.
//!
//! Two sources, one job. qBittorrent hands its library over through its Web
//! API (a list, then one `.torrent` per hash); Transmission cannot, so its
//! config folder is read instead (`torrents/` beside `resume/`). Both are
//! turned into the same `Candidate` list, previewed by the same function and
//! imported by the same runner, so the two wizards cannot drift apart.
//!
//! Runs as a task, not on the request: a qBittorrent with 200k torrents takes
//! a long time to walk, and the call that starts it returns a job id.
//!
//! The order matters and is not obvious. Categories are created first, before
//! a single torrent is added, because a torrent added to a category that does
//! not exist yet lands in the default save path -- and moving it afterwards
//! means copying the data it was supposed to already be sitting on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Where to reach the client we are taking over from.
///
/// Kept in memory by a finished job that had failures, so "retry the failed
/// ones" does not ask for the password again. Never written anywhere, and gone
/// with the next import or a restart.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct QbitCreds {
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

/// One torrent as qBittorrent describes it (`/api/v2/torrents/info`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct QbitTorrent {
    pub hash: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub save_path: String,
    /// The file or folder the torrent's data is, absolute. Empty on qBit
    /// older than 4.2, where `save_path`/`name` is the same thing.
    #[serde(default)]
    pub content_path: String,
    #[serde(default)]
    pub progress: f64,
    /// `pausedUP`, `stoppedDL`, `uploading`... Only the paused/stopped family
    /// matters here.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub uploaded: u64,
    #[serde(default)]
    pub downloaded: u64,
    /// Comma-separated, as qBit sends it.
    #[serde(default)]
    pub tags: String,
}

/// How far an import has got, as the wizard's progress screen reads it.
///
/// `done` counts every torrent dealt with, whatever happened to it, so the bar
/// reaches the end; the four outcomes say what that was.
#[derive(Default)]
pub struct Progress {
    pub phase: std::sync::RwLock<String>,
    pub total: AtomicUsize,
    pub done: AtomicUsize,
    /// Added complete, data trusted, seeding.
    pub seeded: AtomicUsize,
    /// Added incomplete (or with its data not found): checked, then resumed.
    pub downloading: AtomicUsize,
    /// Already in Hydranos.
    pub skipped: AtomicUsize,
    pub failed: AtomicUsize,
    /// Of the added ones, how many were left stopped (the wizard's default).
    /// The outcome counts above say what the data was; this says whether it
    /// is announcing -- 4.3 printed "7821 seeding" over 7997 stopped torrents.
    pub stopped: AtomicUsize,
    /// Each torrent that did not go in, and why. What the "retry" acts on.
    pub failures: std::sync::Mutex<Vec<Failure>>,
    /// What a retry needs: the failed candidates and how to reach their
    /// source. Taken (not cloned) by the retry, so the password does not
    /// outlive the job that needed it.
    pub retry: std::sync::Mutex<Option<RetryKit>>,
    pub finished: AtomicBool,
    pub error: std::sync::RwLock<String>,
    pub current: std::sync::RwLock<String>,
}

/// One torrent an import could not add.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Failure {
    pub name: String,
    pub hash: String,
    pub error: String,
}

/// Everything a retry of the failed torrents needs.
#[derive(Debug, Clone)]
pub struct RetryKit {
    pub cands: Vec<Candidate>,
    /// Set when the candidates come from qBittorrent.
    pub creds: Option<QbitCreds>,
    pub choices: Choices,
}

/// At most this many failures travel in a status frame: the count is exact,
/// the list is for reading, and a broken mapping can fail a whole library.
const FAILURES_SHOWN: usize = 200;

impl Progress {
    pub fn set_phase(&self, phase: &str) {
        *self.phase.write().unwrap() = phase.to_string();
    }

    /// End the job on an error the whole import cannot get past.
    pub fn fail(&self, error: String) {
        *self.error.write().unwrap() = error;
        self.set_phase("error");
        self.finished.store(true, Ordering::Relaxed);
    }

    pub fn running(&self) -> bool {
        !self.finished.load(Ordering::Relaxed)
    }

    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "phase": self.phase.read().unwrap().clone(),
            "total": self.total.load(Ordering::Relaxed),
            "done": self.done.load(Ordering::Relaxed),
            "seeded": self.seeded.load(Ordering::Relaxed),
            "downloading": self.downloading.load(Ordering::Relaxed),
            "skipped": self.skipped.load(Ordering::Relaxed),
            "failed": self.failed.load(Ordering::Relaxed),
            "stopped": self.stopped.load(Ordering::Relaxed),
            "failures": self.failures.lock().unwrap().iter().take(FAILURES_SHOWN).collect::<Vec<_>>(),
            "retryable": self.retry.lock().unwrap().is_some(),
            "finished": self.finished.load(Ordering::Relaxed),
            "error": self.error.read().unwrap().clone(),
            "current": self.current.read().unwrap().clone(),
        })
    }
}

/// A logged-in qBittorrent Web API session.
pub struct Qbit {
    base: String,
    client: reqwest::Client,
    /// `SID=...` or, since 5.x, `QBT_SID_<port>=...`: sent back by hand.
    /// Behind a lock because it is renewed mid-import: qBit expires a session
    /// after an hour idle by default, and a large library takes longer.
    cookie: std::sync::RwLock<String>,
    creds: QbitCreds,
    /// First wait between two export attempts; doubles each time.
    backoff: std::time::Duration,
}

/// Attempts at one `.torrent` before the torrent is counted as failed.
const EXPORT_ATTEMPTS: u32 = 4;

/// What to do after a failed export.
#[derive(Debug, PartialEq, Eq)]
enum Retry {
    /// Transient: the connection, a 5xx, a 429. Wait and try again.
    Later,
    /// The session is gone (401/403): log in again, then try again.
    Relogin,
    /// Asking again will not change the answer.
    Never,
}

/// How an export status is handled.
///
/// A 409 from qBittorrent's export is "no metadata yet": a magnet that never
/// finished fetching its info dict. Nothing to import until it has.
fn export_retry(status: u16) -> Retry {
    match status {
        401 | 403 => Retry::Relogin,
        429 | 500..=599 => Retry::Later,
        _ => Retry::Never,
    }
}

/// Did qBittorrent accept the login?
///
/// Two protocols in the wild, measured against 5.2.3 on 2026-10-02: up to 4.x
/// a 200 with the body `Ok.` (and `Fails.`, also with a 200, for a refusal);
/// 5.x answers a 204 with an EMPTY body, and a 401 for a refusal. Matching on
/// "Ok." alone refused every correct password on 5.x.
fn login_accepted(status: u16, body: &str) -> bool {
    (200..300).contains(&status) && body.trim() != "Fails."
}

impl Qbit {
    /// Log in and keep the session cookie.
    ///
    /// qBittorrent refuses a login without a Referer matching its own address:
    /// it is a CSRF guard, and without the header the answer is "Fails." with
    /// a 200, which reads as success to anything that only checks the status.
    pub async fn login(creds: &QbitCreds) -> Result<Self, String> {
        let base = creds.url.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err("empty qBittorrent URL".into());
        }
        // A bare host:port is what most people paste; reqwest needs a scheme.
        let base = if base.contains("://") { base } else { format!("http://{base}") };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let cookie = Self::authenticate(&client, &base, creds).await?;
        Ok(Self {
            base,
            client,
            cookie: std::sync::RwLock::new(cookie),
            creds: creds.clone(),
            backoff: std::time::Duration::from_secs(1),
        })
    }

    /// The credentials this session logged in with, for a later retry.
    pub fn creds(&self) -> &QbitCreds {
        &self.creds
    }

    /// Log in again on the same client, after qBit dropped the session.
    async fn relogin(&self) -> Result<(), String> {
        let cookie = Self::authenticate(&self.client, &self.base, &self.creds).await?;
        *self.cookie.write().unwrap() = cookie;
        Ok(())
    }

    /// POST the login form; the session cookie on success.
    async fn authenticate(client: &reqwest::Client, base: &str, creds: &QbitCreds) -> Result<String, String> {
        let body = format!(
            "username={}&password={}",
            urlencoding(&creds.username),
            urlencoding(&creds.password)
        );
        let resp = client
            .post(format!("{base}/api/v2/auth/login"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Referer", base)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("cannot reach qBittorrent at {base}: {e}"))?;

        let status = resp.status().as_u16();
        // The session cookie, carried by hand rather than by a cookie jar:
        // 5.x names it `QBT_SID_<port>`, and a jar keyed on a dotless docker
        // hostname (`qbittorrent:8080`) is exactly where jars disagree.
        let cookie = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .filter(|kv| kv.contains("SID"))
            .collect::<Vec<_>>()
            .join("; ");
        let text = resp.text().await.unwrap_or_default();
        if !login_accepted(status, &text) {
            let why = if text.trim().is_empty() { format!("http {status}") } else { text.trim().to_string() };
            return Err(format!("qBittorrent refused the login: {why}"));
        }
        Ok(cookie)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.get(format!("{}{path}", self.base)).header("Referer", &self.base);
        let cookie = self.cookie.read().unwrap().clone();
        if cookie.is_empty() { req } else { req.header(reqwest::header::COOKIE, cookie) }
    }

    /// Every torrent it holds.
    pub async fn torrents(&self) -> Result<Vec<QbitTorrent>, String> {
        let resp = self.get("/api/v2/torrents/info").send().await.map_err(|e| format!("torrents/info: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("torrents/info: http {}", resp.status()));
        }
        resp.json().await.map_err(|e| format!("torrents/info body: {e}"))
    }

    /// The .torrent file for one hash.
    ///
    /// Retried: on a library of thousands, a qBittorrent that is busy (or
    /// restarting, or behind a flaky VPN) drops a few requests, and 4.3 counted
    /// each of those as a torrent that could not be imported. A session that
    /// expired mid-import is renewed rather than failing everything after it.
    pub async fn export(&self, hash: &str) -> Result<Vec<u8>, String> {
        let mut last = String::new();
        let mut wait = self.backoff;
        for attempt in 1..=EXPORT_ATTEMPTS {
            let (err, retry) = match self.export_once(hash).await {
                Ok(bytes) => return Ok(bytes),
                Err(x) => x,
            };
            last = err;
            match retry {
                Retry::Never => return Err(last),
                Retry::Relogin => {
                    if let Err(e) = self.relogin().await {
                        last = format!("{last}; login again: {e}");
                    }
                }
                Retry::Later => {}
            }
            if attempt < EXPORT_ATTEMPTS {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
        }
        Err(format!("{last} (gave up after {EXPORT_ATTEMPTS} attempts)"))
    }

    async fn export_once(&self, hash: &str) -> Result<Vec<u8>, (String, Retry)> {
        let resp = self
            .get(&format!("/api/v2/torrents/export?hash={}", urlencoding(hash)))
            .send()
            .await
            .map_err(|e| (format!("export {hash}: {e}"), Retry::Later))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let why = if status == 409 { " (no metadata yet in qBittorrent)" } else { "" };
            return Err((format!("export {hash}: http {status}{why}"), export_retry(status)));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| (format!("export {hash} body: {e}"), Retry::Later))
    }
}

/// Where a candidate's `.torrent` comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Exported from qBittorrent by hash, at import time.
    Qbit(String),
    /// A file already on disk (Transmission's `torrents/`).
    File(PathBuf),
}

/// One torrent to take over, whichever client it comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    /// Empty: none.
    pub category: String,
    /// Where the source client keeps it, in the source client's view.
    pub save_path: String,
    /// The torrent's file or folder, in the source client's view: what the
    /// data check looks for.
    pub content: String,
    pub complete: bool,
    /// Stopped in the source client. Stays stopped here.
    pub stopped: bool,
    pub uploaded: u64,
    pub downloaded: u64,
    pub tags: Vec<String>,
    pub source: Source,
}

impl Candidate {
    pub fn from_qbit(t: &QbitTorrent) -> Self {
        let content = if !t.content_path.is_empty() {
            t.content_path.clone()
        } else {
            join(&t.save_path, &t.name)
        };
        Self {
            name: t.name.clone(),
            category: t.category.clone(),
            save_path: t.save_path.clone(),
            content,
            // qBit reports 1 exactly when every wanted piece is there.
            complete: t.progress >= 1.0,
            stopped: t.state.starts_with("paused") || t.state.starts_with("stopped"),
            uploaded: t.uploaded,
            downloaded: t.downloaded,
            tags: t.tags.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
            source: Source::Qbit(t.hash.clone()),
        }
    }
}

fn join(dir: &str, name: &str) -> String {
    Path::new(dir).join(name).to_string_lossy().into_owned()
}

/// A category the import creates, and where it points.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CategoryPlan {
    pub name: String,
    pub save_path: String,
}

/// The categories a set of torrents needs, and the save path each one implies.
///
/// Built before anything is added. A torrent added to a category that does not
/// exist yet goes to the default path, and putting it right afterwards means
/// moving the data it should already have been sitting on.
pub fn categories_needed(cands: &[Candidate]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for c in cands {
        if c.category.is_empty() || c.save_path.is_empty() {
            continue;
        }
        // First one wins: qBittorrent allows a per-torrent path override, and
        // taking the last would let one stray torrent redefine the category
        // for every other torrent in it.
        out.entry(c.category.clone()).or_insert_with(|| c.save_path.clone());
    }
    out
}

/// At most this many folders are offered for mapping: past that the list is
/// not something anyone edits, so it is folded onto parent folders.
const MAX_PREFIXES: usize = 40;

/// The folders the wizard offers to remap: the distinct save paths, folded
/// onto their parents until the list is short enough to read.
pub fn path_prefixes<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut set: BTreeSet<String> =
        paths.filter(|p| !p.is_empty()).map(|p| p.trim_end_matches('/').to_string()).collect();
    while set.len() > MAX_PREFIXES {
        let folded: BTreeSet<String> = set
            .iter()
            .map(|p| {
                Path::new(p)
                    .parent()
                    .map(|q| q.to_string_lossy().into_owned())
                    .filter(|q| !q.is_empty())
                    .unwrap_or_else(|| p.clone())
            })
            .collect();
        if folded == set {
            break;
        }
        set = folded;
    }
    // A folder inside another one on the list is covered by it.
    let all: Vec<String> = set.iter().cloned().collect();
    set.retain(|p| !all.iter().any(|q| q != p && under(p, q)));
    set.into_iter().collect()
}

/// `p` is `dir` or inside it, on a path-component boundary: `/data/films2` is
/// not under `/data/films`.
fn under(p: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    p == dir || (p.starts_with(dir) && p[dir.len()..].starts_with('/')) || dir.is_empty()
}

/// Rewrite a source-side path to what Hydranos sees, by the longest mapped
/// folder it is under. Unmapped paths come back unchanged.
pub fn map_path(p: &str, map: &BTreeMap<String, String>) -> String {
    let best = map
        .iter()
        .filter(|(from, to)| !from.is_empty() && !to.is_empty() && under(p, from))
        .max_by_key(|(from, _)| from.trim_end_matches('/').len());
    match best {
        Some((from, to)) => {
            let from = from.trim_end_matches('/');
            format!("{}{}", to.trim_end_matches('/'), &p[from.len()..])
        }
        None => p.to_string(),
    }
}

/// Torrents whose data is looked for in the preview. A sample: on a library of
/// a million, statting every payload is minutes of disk on a cold pool.
const DATA_SAMPLE: usize = 64;

/// What the wizard shows before anything is touched.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Preview {
    pub total: usize,
    pub completed: usize,
    pub incomplete: usize,
    pub stopped: usize,
    pub carried_uploaded_bytes: u64,
    pub categories: Vec<CategoryPlan>,
    pub path_prefixes: Vec<String>,
    pub data_checked: usize,
    pub data_found: usize,
    /// Files that could not be read (Transmission): skipped.
    pub problems: Vec<String>,
    /// Torrents with no resume file, so no save path (Transmission): skipped.
    pub without_resume: usize,
}

pub fn preview(cands: &[Candidate], exists: impl Fn(&Path) -> bool) -> Preview {
    let mut p = Preview { total: cands.len(), ..Default::default() };
    for c in cands {
        if c.complete {
            p.completed += 1;
        } else {
            p.incomplete += 1;
        }
        p.stopped += c.stopped as usize;
        p.carried_uploaded_bytes += c.uploaded;
    }
    p.categories = categories_needed(cands)
        .into_iter()
        .map(|(name, save_path)| CategoryPlan { name, save_path })
        .collect();
    p.path_prefixes = path_prefixes(cands.iter().map(|c| c.save_path.as_str()));
    // Spread over the whole list rather than its head: the head is usually
    // one folder, and one folder says nothing about the others.
    let step = (cands.len() / DATA_SAMPLE).max(1);
    for c in cands.iter().step_by(step).take(DATA_SAMPLE) {
        p.data_checked += 1;
        p.data_found += exists(Path::new(&c.content)) as usize;
    }
    p
}

/// What the wizard chose, applied to every torrent.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Choices {
    #[serde(default)]
    pub path_map: BTreeMap<String, String>,
    /// Defaults to stopped: an import that starts announcing a whole library
    /// on its own is the one nobody can take back.
    #[serde(default = "yes")]
    pub start_stopped: bool,
}

fn yes() -> bool {
    true
}

impl Default for Choices {
    fn default() -> Self {
        Self { path_map: BTreeMap::new(), start_stopped: true }
    }
}

/// How one candidate is added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddPlan {
    pub save_path: String,
    pub paused: bool,
    /// Trust the data, skip the hash check.
    pub seed_mode: bool,
}

/// Decide how one torrent goes in.
///
/// ⚠ Trust (`seed_mode`) needs BOTH "the source said complete" AND "the data
/// is where we will look". Complete in qBit but not found here means a mapping
/// is wrong; trusting it would announce a torrent as a seed that cannot serve
/// one byte. Without trust it is added for a check, which finds what is there.
pub fn plan(c: &Candidate, choices: &Choices, data_present: bool) -> AddPlan {
    AddPlan {
        save_path: map_path(&c.save_path, &choices.path_map),
        paused: choices.start_stopped || c.stopped,
        seed_mode: c.complete && data_present,
    }
}

/// How one add ended, as the runner counts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    Seeded,
    Resumed,
    AlreadyThere,
}

/// What the runner hands the caller for each torrent.
pub struct AddRequest {
    pub bytes: Vec<u8>,
    pub category: String,
    pub tags: String,
    pub plan: AddPlan,
    pub uploaded: u64,
    pub downloaded: u64,
}

pub type AddFn = Arc<dyn Fn(AddRequest) -> Result<Added, String> + Send + Sync>;
pub type CategoryFn = Arc<dyn Fn(&BTreeMap<String, String>) + Send + Sync>;

/// Run one import to completion: categories, then every torrent in order.
///
/// The adds and the filesystem checks run on the blocking pool: each one
/// takes the store lock and may stat a cold pool, and a library is hundreds of
/// thousands of them.
pub async fn run_job(
    cands: Vec<Candidate>,
    qbit: Option<Qbit>,
    choices: Choices,
    progress: Arc<Progress>,
    ensure_categories: CategoryFn,
    add: AddFn,
) {
    progress.total.store(cands.len(), Ordering::Relaxed);

    progress.set_phase("categories");
    let cats: BTreeMap<String, String> = categories_needed(&cands)
        .into_iter()
        .map(|(name, path)| (name, map_path(&path, &choices.path_map)))
        .collect();
    {
        let ensure = ensure_categories.clone();
        let cats = cats.clone();
        let _ = tokio::task::spawn_blocking(move || ensure(&cats)).await;
    }

    progress.set_phase("torrents");
    let mut failed_cands = Vec::new();
    for c in cands {
        *progress.current.write().unwrap() = c.name.clone();
        let stays_stopped = choices.start_stopped || c.stopped;
        // Kept whole for a retry; cheap next to the export it follows.
        let retry_copy = c.clone();
        let bytes = match &c.source {
            Source::Qbit(hash) => match &qbit {
                Some(q) => q.export(hash).await,
                None => Err("no qBittorrent session".into()),
            },
            Source::File(path) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || std::fs::read(&path).map_err(|e| e.to_string()))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            }
        };
        let outcome = match bytes {
            Err(e) => Err(e),
            Ok(bytes) => {
                let add = add.clone();
                let choices = choices.clone();
                tokio::task::spawn_blocking(move || {
                    let content = map_path(&c.content, &choices.path_map);
                    let present = Path::new(&content).exists();
                    add(AddRequest {
                        bytes,
                        category: c.category.clone(),
                        tags: c.tags.join(","),
                        plan: plan(&c, &choices, present),
                        uploaded: c.uploaded,
                        downloaded: c.downloaded,
                    })
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            }
        };
        if matches!(outcome, Ok(Added::Seeded) | Ok(Added::Resumed)) && stays_stopped {
            progress.stopped.fetch_add(1, Ordering::Relaxed);
        }
        match outcome {
            Ok(Added::Seeded) => progress.seeded.fetch_add(1, Ordering::Relaxed),
            Ok(Added::Resumed) => progress.downloading.fetch_add(1, Ordering::Relaxed),
            Ok(Added::AlreadyThere) => progress.skipped.fetch_add(1, Ordering::Relaxed),
            Err(e) => {
                tracing::warn!(name = %retry_copy.name, error = %e, "import: torrent not added");
                progress.failures.lock().unwrap().push(Failure {
                    name: retry_copy.name.clone(),
                    hash: match &retry_copy.source {
                        Source::Qbit(h) => h.clone(),
                        Source::File(p) => p.to_string_lossy().into_owned(),
                    },
                    error: e,
                });
                failed_cands.push(retry_copy);
                progress.failed.fetch_add(1, Ordering::Relaxed)
            }
        };
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    if !failed_cands.is_empty() {
        *progress.retry.lock().unwrap() = Some(RetryKit {
            cands: failed_cands,
            creds: qbit.as_ref().map(|q| q.creds().clone()),
            choices: choices.clone(),
        });
    }
    progress.set_phase("done");
    progress.finished.store(true, Ordering::Relaxed);
}

/// Percent-encode one form or query value.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(category: &str, save_path: &str) -> Candidate {
        Candidate {
            name: "x".into(),
            category: category.into(),
            save_path: save_path.into(),
            content: join(save_path, "x"),
            complete: true,
            stopped: false,
            uploaded: 0,
            downloaded: 0,
            tags: vec![],
            source: Source::Qbit("aa".into()),
        }
    }

    #[test]
    fn a_category_takes_the_first_path_it_is_seen_with() {
        // qBittorrent allows a per-torrent override. Taking the last would let
        // one stray torrent redefine where every other torrent of that
        // category is supposed to live.
        let rows = vec![c("Films", "/data/films"), c("Films", "/tmp/oneoff")];
        let cats = categories_needed(&rows);
        assert_eq!(cats.get("Films").map(String::as_str), Some("/data/films"));
    }

    #[test]
    fn a_torrent_without_a_category_needs_none_created() {
        let rows = vec![c("", "/data/loose"), c("Books", "")];
        assert!(categories_needed(&rows).is_empty());
    }

    #[test]
    fn a_password_is_escaped_before_it_is_posted() {
        // A password with an ampersand would otherwise end the field and turn
        // its tail into another form key.
        assert_eq!(urlencoding("p@ss&word"), "p%40ss%26word");
    }

    #[test]
    fn both_login_protocols_are_understood() {
        assert!(login_accepted(200, "Ok."), "4.x");
        assert!(login_accepted(204, ""), "5.x: 204, empty body");
        assert!(!login_accepted(200, "Fails."), "4.x refusal, with a 200");
        assert!(!login_accepted(401, "Unauthorized"), "5.x refusal");
        assert!(!login_accepted(403, ""));
    }

    #[test]
    fn qbit_rows_become_candidates_with_their_state() {
        let t = QbitTorrent {
            hash: "ab".into(),
            name: "Film".into(),
            save_path: "/downloads/films".into(),
            progress: 0.5,
            state: "pausedDL".into(),
            uploaded: 7,
            tags: "a, b,,".into(),
            ..Default::default()
        };
        let got = Candidate::from_qbit(&t);
        assert!(!got.complete);
        assert!(got.stopped);
        assert_eq!(got.content, "/downloads/films/Film", "no content_path: save_path/name");
        assert_eq!(got.tags, vec!["a", "b"]);
        assert_eq!(got.source, Source::Qbit("ab".into()));
        let done = Candidate::from_qbit(&QbitTorrent { progress: 1.0, state: "uploading".into(), ..t });
        assert!(done.complete && !done.stopped);
    }

    #[test]
    fn a_path_is_mapped_by_the_longest_folder_on_a_component_boundary() {
        let map: BTreeMap<String, String> = [
            ("/downloads".to_string(), "/data".to_string()),
            ("/downloads/films/".to_string(), "/films".to_string()),
        ]
        .into();
        assert_eq!(map_path("/downloads/films/A", &map), "/films/A");
        assert_eq!(map_path("/downloads/films", &map), "/films");
        assert_eq!(map_path("/downloads/series/B", &map), "/data/series/B");
        assert_eq!(map_path("/downloads2/x", &map), "/downloads2/x", "not a component boundary");
        assert_eq!(map_path("/elsewhere", &map), "/elsewhere");
    }

    #[test]
    fn prefixes_fold_onto_parents_and_drop_nested_ones() {
        let got = path_prefixes(["/d/films", "/d/films/hd", "/e/x"].into_iter());
        assert_eq!(got, vec!["/d/films", "/e/x"]);
        let many: Vec<String> = (0..100).map(|i| format!("/data/lib/{i}")).collect();
        assert_eq!(path_prefixes(many.iter().map(String::as_str)), vec!["/data/lib"]);
    }

    #[test]
    fn complete_data_is_trusted_only_where_it_was_found() {
        let mut x = c("", "/q/films");
        let ch = Choices {
            path_map: [("/q".to_string(), "/data".to_string())].into(),
            start_stopped: false,
        };
        assert_eq!(
            plan(&x, &ch, true),
            AddPlan { save_path: "/data/films".into(), paused: false, seed_mode: true }
        );
        assert!(!plan(&x, &ch, false).seed_mode, "complete but not found: check, never trust");
        x.complete = false;
        assert!(!plan(&x, &ch, true).seed_mode, "partial: check and resume");
        x.stopped = true;
        assert!(plan(&x, &ch, true).paused, "stopped in the source stays stopped");
    }

    #[test]
    fn an_import_defaults_to_stopped() {
        let ch: Choices = serde_json::from_str(r#"{"url":"x"}"#).unwrap();
        assert!(ch.start_stopped);
    }

    #[test]
    fn the_preview_counts_and_samples_the_data() {
        let mut rows: Vec<Candidate> = (0..10).map(|i| c("Films", &format!("/d/{i}"))).collect();
        rows[0].complete = false;
        rows[1].stopped = true;
        rows[2].uploaded = 100;
        let p = preview(&rows, |path| path.starts_with("/d/1"));
        assert_eq!((p.total, p.completed, p.incomplete, p.stopped), (10, 9, 1, 1));
        assert_eq!(p.carried_uploaded_bytes, 100);
        assert_eq!(p.categories, vec![CategoryPlan { name: "Films".into(), save_path: "/d/0".into() }]);
        assert_eq!((p.data_checked, p.data_found), (10, 1));
    }
}

// ---------------------------------------------------------------------------
// Transmission
// ---------------------------------------------------------------------------

/// What a Transmission `.resume` file says about one torrent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResumeFacts {
    pub destination: String,
    pub uploaded: u64,
    pub downloaded: u64,
    pub paused: bool,
    pub labels: Vec<String>,
    pub complete: bool,
}

/// Read a Transmission `.resume` file (bencode).
///
/// Completion is read from the `progress` dict, which has spelt "everything"
/// three ways across versions: `have`, `blocks` or `pieces` set to the string
/// `all`. A `pieces` bitfield counts too when every one of the torrent's
/// pieces is set. Anything else reads as incomplete, which only costs a check.
pub fn resume_facts(data: &[u8], num_pieces: u32) -> Option<ResumeFacts> {
    let value = typhon_engine::torrent::metainfo::bencode_decode(data).ok()?;
    let dict = value.as_dict()?;
    let int = |k: &str| dict.get(k).and_then(|v| v.as_int()).unwrap_or(0);
    let mut out = ResumeFacts {
        destination: dict
            .get("destination")
            .and_then(|v| v.as_bytes())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default(),
        uploaded: int("uploaded").max(0) as u64,
        downloaded: int("downloaded").max(0) as u64,
        paused: int("paused") != 0,
        labels: dict
            .get("labels")
            .and_then(|v| v.as_list())
            .map(|l| l.iter().filter_map(|x| x.as_string()).map(String::from).collect())
            .unwrap_or_default(),
        complete: false,
    };
    if let Some(progress) = dict.get("progress").and_then(|v| v.as_dict()) {
        let says_all = ["have", "blocks", "pieces"]
            .iter()
            .any(|k| progress.get(k).and_then(|v| v.as_bytes()) == Some(b"all".as_slice()));
        let bits_all = progress
            .get("pieces")
            .and_then(|v| v.as_bytes())
            .filter(|b| b != b"all" && num_pieces > 0)
            .map(|b| (0..num_pieces as usize).all(|i| b.get(i / 8).is_some_and(|byte| byte & (0x80 >> (i % 8)) != 0)))
            .unwrap_or(false);
        out.complete = says_all || bits_all;
    }
    Some(out)
}

/// Read the destination out of a Transmission `.resume` file.
pub fn destination_from_resume(data: &[u8]) -> Option<String> {
    resume_facts(data, 0).map(|f| f.destination).filter(|d| !d.is_empty())
}

/// A Transmission config folder, read.
#[derive(Debug, Default)]
pub struct TransmissionScan {
    pub cands: Vec<Candidate>,
    pub problems: Vec<String>,
    pub without_resume: usize,
}

/// Read `torrents/` and `resume/` under a Transmission config folder.
///
/// The two are paired by file stem (`Name.a1b2c3d4.torrent` beside
/// `Name.a1b2c3d4.resume`, or `<hash>.torrent` since 4.0). A torrent whose
/// resume file is missing, or names no destination, is counted and skipped,
/// not guessed at: the resume file is the only thing that says where the data
/// lives, and a default path would re-download a library already on disk.
pub fn scan_transmission(dir: &Path, categories_from_dirs: bool, import_labels: bool) -> Result<TransmissionScan, String> {
    let tdir = dir.join("torrents");
    let entries = std::fs::read_dir(&tdir)
        .map_err(|e| format!("no torrents folder at {}: {e}", tdir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "torrent"))
        .collect();
    files.sort();

    let mut out = TransmissionScan::default();
    for path in files {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let meta = match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| typhon_engine::torrent::metainfo::parse_torrent_bytes(&b))
        {
            Ok(m) => m,
            Err(e) => {
                out.problems.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let facts = std::fs::read(dir.join("resume").join(format!("{stem}.resume")))
            .ok()
            .and_then(|b| resume_facts(&b, meta.num_pieces()));
        let Some(f) = facts.filter(|f| !f.destination.is_empty()) else {
            out.without_resume += 1;
            continue;
        };
        let category = if categories_from_dirs {
            Path::new(&f.destination)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        out.cands.push(Candidate {
            content: join(&f.destination, &meta.name),
            name: meta.name.clone(),
            category,
            save_path: f.destination.clone(),
            complete: f.complete,
            stopped: f.paused,
            uploaded: f.uploaded,
            downloaded: f.downloaded,
            tags: if import_labels { f.labels.clone() } else { vec![] },
            source: Source::File(path),
        });
    }
    Ok(out)
}

/// Upper bounds on an uploaded config zip. A Transmission folder is `.torrent`
/// and `.resume` files: a million of them is a few GB, and an archive past
/// this is not one.
const ZIP_MAX_ENTRIES: usize = 2_000_000;
const ZIP_MAX_BYTES: u64 = 8 << 30;

/// Unpack an uploaded Transmission config folder and find where it starts.
///
/// The zip may hold the folder itself or its contents; the answer is the
/// first folder (at most two levels down) that has a `torrents/` inside.
/// Entries escaping the target (`../`, absolute names) are refused, not
/// skipped: an archive carrying one is not a Transmission folder.
pub fn unpack_zip(bytes: &[u8], into: &Path) -> Result<PathBuf, String> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("not a zip: {e}"))?;
    if zip.len() > ZIP_MAX_ENTRIES {
        return Err(format!("{} entries: not a Transmission folder", zip.len()));
    }
    let mut written: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("zip entry {i}: {e}"))?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(format!("zip entry {:?} points outside the archive", entry.name()));
        };
        let target = into.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        let n = std::io::copy(&mut (&mut entry).take(ZIP_MAX_BYTES - written + 1), &mut out)
            .map_err(|e| format!("{}: {e}", target.display()))?;
        written += n;
        if written > ZIP_MAX_BYTES {
            return Err("archive unpacks to more than 8 GB: not a Transmission folder".into());
        }
    }
    find_config_root(into).ok_or_else(|| "no torrents/ folder in the archive".to_string())
}

fn find_config_root(dir: &Path) -> Option<PathBuf> {
    if dir.join("torrents").is_dir() {
        return Some(dir.to_path_buf());
    }
    let mut subs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    subs.sort();
    subs.iter()
        .find(|p| p.join("torrents").is_dir())
        .cloned()
        .or_else(|| subs.iter().find_map(|p| {
            let mut inner: Vec<PathBuf> = std::fs::read_dir(p).ok()?.filter_map(|e| e.ok().map(|e| e.path())).collect();
            inner.sort();
            inner.into_iter().find(|q| q.join("torrents").is_dir())
        }))
}

#[cfg(test)]
mod transmission_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn the_destination_is_read_out_of_the_bencode() {
        // d11:destination10:/data/dl1:xi1ee -- destination plus a key we ignore
        let data = b"d11:destination8:/data/dl1:xi1ee";
        assert_eq!(destination_from_resume(data).as_deref(), Some("/data/dl"));
        assert_eq!(destination_from_resume(b"d1:xi1ee"), None);
        assert_eq!(destination_from_resume(b"not bencode"), None);
    }

    #[test]
    fn resume_facts_read_progress_counters_and_labels() {
        let data = b"d11:destination5:/data10:downloadedi20e6:labelsl1:a1:be6:pausedi1e8:progressd6:blocks3:alle8:uploadedi10ee";
        let f = resume_facts(data, 4).unwrap();
        assert_eq!(f.destination, "/data");
        assert_eq!((f.uploaded, f.downloaded, f.paused, f.complete), (10, 20, true, true));
        assert_eq!(f.labels, vec!["a", "b"]);
        // A pieces bitfield: 4 pieces, all set (0xF0) is complete; 0xE0 is not.
        let bits = |b: u8| {
            let mut v = b"d11:destination1:/8:progressd6:pieces1:".to_vec();
            v.push(b);
            v.extend_from_slice(b"ee");
            v
        };
        assert!(resume_facts(&bits(0xF0), 4).unwrap().complete);
        assert!(!resume_facts(&bits(0xE0), 4).unwrap().complete);
        assert!(!resume_facts(b"d11:destination1:/e", 4).unwrap().complete, "no progress: check");
    }

    fn torrent(name: &str) -> Vec<u8> {
        let piece = [0u8; 20];
        let mut v = format!("d4:infod6:lengthi5e4:name{}:{}12:piece lengthi16384e6:pieces20:", name.len(), name).into_bytes();
        v.extend_from_slice(&piece);
        v.extend_from_slice(b"ee");
        v
    }

    #[test]
    fn a_config_folder_is_paired_by_stem_and_a_missing_resume_is_skipped() {
        let dir = std::env::temp_dir().join(format!("tr-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("torrents")).unwrap();
        std::fs::create_dir_all(dir.join("resume")).unwrap();
        std::fs::write(dir.join("torrents/A.a1.torrent"), torrent("Alpha")).unwrap();
        std::fs::write(dir.join("torrents/B.b2.torrent"), torrent("Bravo")).unwrap();
        std::fs::write(dir.join("torrents/C.c3.torrent"), b"junk").unwrap();
        std::fs::write(
            dir.join("resume/A.a1.resume"),
            b"d11:destination11:/data/films6:labelsl1:xe8:progressd4:have3:alle8:uploadedi9ee",
        )
        .unwrap();
        let s = scan_transmission(&dir, true, true).unwrap();
        assert_eq!(s.cands.len(), 1);
        let a = &s.cands[0];
        assert_eq!((a.name.as_str(), a.category.as_str(), a.save_path.as_str()), ("Alpha", "films", "/data/films"));
        assert_eq!(a.content, "/data/films/Alpha");
        assert!(a.complete);
        assert_eq!((a.uploaded, a.tags.clone()), (9, vec!["x".to_string()]));
        // Guessing a path would make the engine re-download a library that is
        // already sitting on disk.
        assert_eq!(s.without_resume, 1);
        assert_eq!(s.problems.len(), 1, "an unreadable .torrent is reported");
        let plain = scan_transmission(&dir, false, false).unwrap();
        assert!(plain.cands[0].category.is_empty() && plain.cands[0].tags.is_empty());
        assert!(scan_transmission(&dir.join("nope"), true, true).unwrap_err().contains("no torrents folder"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            for (name, data) in entries {
                z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(data).unwrap();
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn an_uploaded_zip_is_unpacked_and_its_config_root_found() {
        let into = std::env::temp_dir().join(format!("tr-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&into);
        std::fs::create_dir_all(&into).unwrap();
        let z = zip_of(&[("transmission/torrents/A.torrent", b"x"), ("transmission/resume/A.resume", b"y")]);
        let root = unpack_zip(&z, &into).unwrap();
        assert_eq!(root, into.join("transmission"));
        assert!(root.join("resume/A.resume").exists());
        let _ = std::fs::remove_dir_all(&into);

        std::fs::create_dir_all(&into).unwrap();
        let evil = zip_of(&[("../escape.txt", b"x")]);
        assert!(unpack_zip(&evil, &into).unwrap_err().contains("outside"));
        assert!(!into.parent().unwrap().join("escape.txt").exists());
        assert!(unpack_zip(&zip_of(&[("readme.txt", b"x")]), &into).unwrap_err().contains("no torrents/"));
        let _ = std::fs::remove_dir_all(&into);
    }
}

/// The import against a fake qBittorrent: what happens when it misbehaves.
#[cfg(test)]
mod qbit_session_tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        logins: AtomicUsize,
        calls: Mutex<HashMap<String, usize>>,
    }

    /// Hash names a behaviour: `flaky` fails twice with a 500, `expired`
    /// refuses the first session, `magnet` is qBit's 409, `down` never answers
    /// anything but 503.
    async fn export(
        State(f): State<Arc<Fake>>,
        Query(q): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        let hash = q.get("hash").cloned().unwrap_or_default();
        let n = {
            let mut calls = f.calls.lock().unwrap();
            let n = calls.entry(hash.clone()).or_default();
            *n += 1;
            *n
        };
        let cookie = headers.get("cookie").and_then(|v| v.to_str().ok()).unwrap_or("");
        let ok = (StatusCode::OK, b"d4:infode".to_vec()).into_response();
        match hash.as_str() {
            "flaky" if n <= 2 => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            "expired" if cookie == "SID=1" => StatusCode::FORBIDDEN.into_response(),
            "magnet" => StatusCode::CONFLICT.into_response(),
            "down" => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            _ => ok,
        }
    }

    async fn login(State(f): State<Arc<Fake>>) -> axum::response::Response {
        let n = f.logins.fetch_add(1, Ordering::Relaxed) + 1;
        ([("set-cookie", format!("SID={n}; HttpOnly"))], "Ok.").into_response()
    }

    async fn fake() -> (Arc<Fake>, Qbit) {
        let f = Arc::new(Fake::default());
        let app = axum::Router::new()
            .route("/api/v2/auth/login", axum::routing::post(login))
            .route("/api/v2/torrents/export", axum::routing::get(export))
            .with_state(f.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let creds = QbitCreds { url: format!("http://{addr}"), username: "u".into(), password: "p".into() };
        let mut q = Qbit::login(&creds).await.expect("login");
        q.backoff = std::time::Duration::from_millis(1);
        (f, q)
    }

    fn calls(f: &Fake, hash: &str) -> usize {
        f.calls.lock().unwrap().get(hash).copied().unwrap_or(0)
    }

    #[tokio::test]
    async fn a_transient_error_is_retried_until_the_torrent_comes() {
        let (f, q) = fake().await;
        assert!(q.export("flaky").await.is_ok());
        assert_eq!(calls(&f, "flaky"), 3);
    }

    #[tokio::test]
    async fn an_expired_session_is_renewed_and_the_export_carries_on() {
        let (f, q) = fake().await;
        assert!(q.export("expired").await.is_ok());
        assert_eq!(f.logins.load(Ordering::Relaxed), 2, "logged in again once");
        assert_eq!(calls(&f, "expired"), 2);
    }

    #[tokio::test]
    async fn a_magnet_without_metadata_is_not_asked_again() {
        let (f, q) = fake().await;
        let err = q.export("magnet").await.unwrap_err();
        assert!(err.contains("no metadata"), "{err}");
        assert_eq!(calls(&f, "magnet"), 1);
    }

    #[tokio::test]
    async fn a_client_that_stays_down_fails_after_a_bounded_number_of_tries() {
        let (f, q) = fake().await;
        let err = q.export("down").await.unwrap_err();
        assert!(err.contains("gave up"), "{err}");
        assert_eq!(calls(&f, "down"), EXPORT_ATTEMPTS as usize);
    }

    fn file_cand(name: &str, path: PathBuf) -> Candidate {
        Candidate {
            name: name.into(),
            category: String::new(),
            save_path: "/nowhere".into(),
            content: "/nowhere".into(),
            complete: true,
            stopped: false,
            uploaded: 0,
            downloaded: 0,
            tags: Vec::new(),
            source: Source::File(path),
        }
    }

    /// The summary says what is stopped, and a failed torrent is kept, whole,
    /// for the retry.
    #[tokio::test]
    async fn failures_are_listed_and_kept_for_a_retry_and_stopped_ones_counted() {
        let dir = std::env::temp_dir().join(format!("imp-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.torrent");
        std::fs::write(&good, b"x").unwrap();
        let cands = vec![
            file_cand("good", good),
            file_cand("missing", dir.join("missing.torrent")),
        ];
        let progress = Arc::new(Progress::default());
        let add: AddFn = Arc::new(|_| Ok(Added::Seeded));
        let cats: CategoryFn = Arc::new(|_| {});
        run_job(cands, None, Choices::default(), progress.clone(), cats, add).await;

        assert_eq!(progress.seeded.load(Ordering::Relaxed), 1);
        assert_eq!(progress.stopped.load(Ordering::Relaxed), 1, "the wizard's default leaves it stopped");
        assert_eq!(progress.failed.load(Ordering::Relaxed), 1);
        let failures = progress.failures.lock().unwrap().clone();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "missing");
        let kit = progress.retry.lock().unwrap().clone().expect("a retry kit");
        assert_eq!(kit.cands.len(), 1);
        assert_eq!(kit.cands[0].name, "missing");
        assert!(kit.creds.is_none(), "a Transmission import has no password to keep");
        let json = progress.as_json();
        assert_eq!(json["retryable"], true);
        assert_eq!(json["failures"][0]["name"], "missing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_server_side_or_session_failure_is_retried() {
        assert_eq!(export_retry(500), Retry::Later);
        assert_eq!(export_retry(429), Retry::Later);
        assert_eq!(export_retry(403), Retry::Relogin);
        assert_eq!(export_retry(409), Retry::Never);
        assert_eq!(export_retry(404), Retry::Never);
    }
}
