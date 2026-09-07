//! Taking a library over from another client.
//!
//! Runs as a task, not on the request: a qBittorrent with 200k torrents takes
//! a long time to walk, and the call that starts it returns a job id.
//!
//! The order matters and is not obvious. Categories are created first, before
//! a single torrent is added, because a torrent added to a category that does
//! not exist yet lands in the default save path -- and moving it afterwards
//! means copying the data it was supposed to already be sitting on.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Where to reach the client we are taking over from.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct QbitCreds {
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

/// One torrent as qBittorrent describes it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct QbitTorrent {
    pub hash: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default, rename = "save_path")]
    pub save_path: String,
    #[serde(default)]
    pub progress: f64,
}

/// How far an import has got.
#[derive(Default)]
pub struct Progress {
    pub total: AtomicUsize,
    pub done: AtomicUsize,
    pub failed: AtomicUsize,
    pub finished: std::sync::atomic::AtomicBool,
    pub error: std::sync::RwLock<String>,
}

impl Progress {
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "total": self.total.load(Ordering::Relaxed),
            "done": self.done.load(Ordering::Relaxed),
            "failed": self.failed.load(Ordering::Relaxed),
            "finished": self.finished.load(Ordering::Relaxed),
            "error": self.error.read().unwrap().clone(),
        })
    }
}

/// A logged-in qBittorrent Web API session.
pub struct Qbit {
    base: String,
    client: reqwest::Client,
}

impl Qbit {
    /// Log in and keep the session cookie.
    ///
    /// qBittorrent refuses a login without a Referer matching its own address:
    /// it is a CSRF guard, and without the header the answer is "Fails." with
    /// a 200, which reads as success to anything that only checks the status.
    pub async fn login(creds: &QbitCreds) -> Result<Self, String> {
        let base = creds.url.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("http client: {e}"))?;

        let body = format!(
            "username={}&password={}",
            urlencoding(&creds.username),
            urlencoding(&creds.password)
        );
        let resp = client
            .post(format!("{base}/api/v2/auth/login"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Referer", &base)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("cannot reach qBittorrent: {e}"))?;

        let text = resp.text().await.unwrap_or_default();
        if text.trim() != "Ok." {
            return Err(format!("qBittorrent refused the login: {}", text.trim()));
        }
        Ok(Self { base, client })
    }

    /// Every torrent it holds.
    pub async fn torrents(&self) -> Result<Vec<QbitTorrent>, String> {
        let resp = self
            .client
            .get(format!("{}/api/v2/torrents/info", self.base))
            .send()
            .await
            .map_err(|e| format!("torrents/info: {e}"))?;
        resp.json().await.map_err(|e| format!("torrents/info body: {e}"))
    }

    /// The .torrent file for one hash.
    pub async fn export(&self, hash: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .client
            .get(format!("{}/api/v2/torrents/export?hash={}", self.base, urlencoding(hash)))
            .send()
            .await
            .map_err(|e| format!("export {hash}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("export {hash}: http {}", resp.status()));
        }
        resp.bytes().await.map(|b| b.to_vec()).map_err(|e| format!("export {hash} body: {e}"))
    }
}

/// The categories a set of torrents needs, and the save path each one implies.
///
/// Built before anything is added. A torrent added to a category that does not
/// exist yet goes to the default path, and putting it right afterwards means
/// moving the data it should already have been sitting on.
pub fn categories_needed(torrents: &[QbitTorrent]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for t in torrents {
        if t.category.is_empty() || t.save_path.is_empty() {
            continue;
        }
        // First one wins: qBittorrent allows a per-torrent path override, and
        // taking the last would let one stray torrent redefine the category
        // for every other torrent in it.
        out.entry(t.category.clone()).or_insert_with(|| t.save_path.clone());
    }
    out
}

/// Run one import to completion.
pub async fn run_import(
    creds: QbitCreds,
    progress: Arc<Progress>,
    mut add: impl FnMut(&QbitTorrent, Vec<u8>) -> Result<(), String>,
) {
    let qbit = match Qbit::login(&creds).await {
        Ok(q) => q,
        Err(e) => {
            *progress.error.write().unwrap() = e;
            progress.finished.store(true, Ordering::Relaxed);
            return;
        }
    };
    let torrents = match qbit.torrents().await {
        Ok(t) => t,
        Err(e) => {
            *progress.error.write().unwrap() = e;
            progress.finished.store(true, Ordering::Relaxed);
            return;
        }
    };
    progress.total.store(torrents.len(), Ordering::Relaxed);

    for t in &torrents {
        match qbit.export(&t.hash).await {
            Ok(bytes) => match add(t, bytes) {
                Ok(()) => {
                    progress.done.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!(name = %t.name, error = %e, "import: cannot add");
                    progress.failed.fetch_add(1, Ordering::Relaxed);
                }
            },
            Err(e) => {
                tracing::warn!(name = %t.name, error = %e, "import: cannot export");
                progress.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
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

    fn t(category: &str, save_path: &str) -> QbitTorrent {
        QbitTorrent {
            hash: "aa".into(),
            name: "x".into(),
            category: category.into(),
            save_path: save_path.into(),
            progress: 1.0,
        }
    }

    #[test]
    fn a_category_takes_the_first_path_it_is_seen_with() {
        // qBittorrent allows a per-torrent override. Taking the last would let
        // one stray torrent redefine where every other torrent of that
        // category is supposed to live.
        let rows = vec![t("Films", "/data/films"), t("Films", "/tmp/oneoff")];
        let cats = categories_needed(&rows);
        assert_eq!(cats.get("Films").map(String::as_str), Some("/data/films"));
    }

    #[test]
    fn a_torrent_without_a_category_needs_none_created() {
        let rows = vec![t("", "/data/loose"), t("Books", "")];
        assert!(categories_needed(&rows).is_empty());
    }

    #[test]
    fn a_password_is_escaped_before_it_is_posted() {
        // A password with an ampersand would otherwise end the field and turn
        // its tail into another form key.
        assert_eq!(urlencoding("p@ss&word"), "p%40ss%26word");
    }
}

// ---------------------------------------------------------------------------
// Transmission
// ---------------------------------------------------------------------------

/// One torrent found in a Transmission data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransmissionEntry {
    /// Path of the .torrent file.
    pub torrent_path: std::path::PathBuf,
    /// Where Transmission put the data, read from the matching .resume.
    pub destination: String,
}

/// Pair the `.torrent` files with their `.resume` siblings.
///
/// Transmission keeps them in two folders, matched by a name that carries a
/// hash suffix -- `Some.Name.a1b2c3d4.torrent` beside
/// `Some.Name.a1b2c3d4.resume`. The stem is the key.
///
/// A torrent whose resume file is missing is skipped, not guessed at: the
/// resume file is the only thing that says where the data lives, and adding it
/// to a default path would make the engine re-download a library that is
/// already on disk.
pub fn pair_transmission(
    torrents: &[(std::path::PathBuf, ())],
    resumes: &std::collections::BTreeMap<String, String>,
) -> (Vec<TransmissionEntry>, Vec<String>) {
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    for (path, _) in torrents {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        match resumes.get(&stem) {
            Some(dest) if !dest.is_empty() => found.push(TransmissionEntry {
                torrent_path: path.clone(),
                destination: dest.clone(),
            }),
            _ => skipped.push(format!("{stem}: no destination in the resume file")),
        }
    }
    (found, skipped)
}

/// Read the destination out of a Transmission `.resume` file.
///
/// The file is bencode with a `destination` key. Everything else in it --
/// progress bitfield, priorities, speed limits -- describes a client we are
/// not becoming, and is deliberately ignored.
pub fn destination_from_resume(data: &[u8]) -> Option<String> {
    let value = typhon_engine::torrent::metainfo::bencode_decode(data).ok()?;
    let dict = value.as_dict()?;
    let raw = dict.get("destination")?.as_bytes()?;
    Some(String::from_utf8_lossy(raw).into_owned())
}

#[cfg(test)]
mod transmission_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn a_torrent_without_its_resume_is_skipped_not_guessed() {
        let torrents = vec![
            (PathBuf::from("/t/A.a1b2.torrent"), ()),
            (PathBuf::from("/t/B.c3d4.torrent"), ()),
        ];
        let mut resumes = BTreeMap::new();
        resumes.insert("A.a1b2".to_string(), "/data/films".to_string());
        let (found, skipped) = pair_transmission(&torrents, &resumes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].destination, "/data/films");
        // Guessing a path would make the engine re-download a library that is
        // already sitting on disk.
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("B.c3d4"));
    }

    #[test]
    fn an_empty_destination_counts_as_missing() {
        let torrents = vec![(PathBuf::from("/t/A.a1b2.torrent"), ())];
        let mut resumes = BTreeMap::new();
        resumes.insert("A.a1b2".to_string(), String::new());
        let (found, skipped) = pair_transmission(&torrents, &resumes);
        assert!(found.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn the_destination_is_read_out_of_the_bencode() {
        // d11:destination10:/data/dl1:xi1ee -- destination plus a key we ignore
        let data = b"d11:destination8:/data/dl1:xi1ee";
        assert_eq!(destination_from_resume(data).as_deref(), Some("/data/dl"));
        assert_eq!(destination_from_resume(b"d1:xi1ee"), None);
        assert_eq!(destination_from_resume(b"not bencode"), None);
    }
}
