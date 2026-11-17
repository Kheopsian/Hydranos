//! Which filesystem holds a torrent's data, so files missing because their
//! disk is not mounted are told from files that were deleted.
//!
//! A mount point with nothing mounted on it is an ordinary empty folder of
//! the filesystem below it. So a torrent's files that are absent while the
//! folder holding them belongs to another filesystem than the one recorded at
//! the last good verdict were not deleted: their disk is not there. Files
//! absent on the very filesystem they were recorded on were deleted.
//!
//! The identity is the filesystem id (`statvfs` `f_fsid`) where the system
//! gives one: ext4, XFS and btrfs derive it from the filesystem UUID, so it
//! survives a reboot and a disk that comes back under another device name.
//! Otherwise it is the device number (`st_dev`). Windows has neither here:
//! a missing drive letter already shows as a missing save path, and the
//! other cases fall back to the folder test in `TorrentState`.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// How long a probe of a disk may take before the disk counts as not
/// answering. A stale network mount can block a `stat` for minutes.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The filesystem holding `path`, or holding its nearest ancestor that
/// exists. `None` when nothing of it exists or the system has no notion of
/// one (Windows).
pub fn volume_id(path: &Path) -> Option<u64> {
    path.ancestors().find_map(volume_of)
}

#[cfg(unix)]
// `f_fsid` is a `c_ulong`: already u64 on 64-bit Linux, 32 bits elsewhere.
#[allow(clippy::useless_conversion)]
fn volume_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(p).ok()?.dev();
    let fsid = nix::sys::statvfs::statvfs(p)
        .map(|s| u64::from(s.filesystem_id()))
        .unwrap_or(0);
    Some(if fsid != 0 { fsid } else { dev })
}

#[cfg(not(unix))]
fn volume_of(_p: &Path) -> Option<u64> {
    None
}

/// The filesystem holding `path` (or its nearest existing ancestor) is a
/// FUSE pool -- mergerfs, rclone and the like -- where one branch can be
/// missing while the mount stays up. Files absent there cannot be told
/// deleted from offline, so they are taken for offline.
pub fn is_pooled(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::statfs::{statfs, FUSE_SUPER_MAGIC};
        path.ancestors()
            .find_map(|p| statfs(p).ok())
            .is_some_and(|s| s.filesystem_type() == FUSE_SUPER_MAGIC)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// Run `f`, which touches the disk, on a thread of its own and wait at most
/// `PROBE_TIMEOUT` for it. `None` when it did not answer in time: the thread
/// is left to finish on its own, since a blocked `stat` cannot be cancelled.
/// One probe per `key` at a time: while one is stuck, asking again answers
/// `None` at once instead of leaving another thread behind.
pub fn probe<T: Send + 'static>(key: PathBuf, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    static IN_FLIGHT: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    {
        let mut g = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains(&key) {
            return None;
        }
        g.push(key.clone());
    }
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("volume-probe".into())
        .spawn(move || {
            let out = f();
            IN_FLIGHT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|k| k != &key);
            let _ = tx.send(out);
        });
    if spawned.is_err() {
        return None;
    }
    rx.recv_timeout(PROBE_TIMEOUT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_missing_path_is_on_the_volume_of_its_nearest_ancestor() {
        let dir = std::env::temp_dir();
        let deep = dir.join("typhon-volume-test-absent/a/b/c.bin");
        assert_eq!(volume_id(&deep), volume_id(&dir));
        assert!(volume_id(&dir).is_some());
    }

    #[test]
    fn a_probe_that_answers_returns_its_answer() {
        assert_eq!(probe("k-answer".into(), || 7), Some(7));
    }

    #[test]
    fn a_stuck_probe_times_out_and_is_not_started_twice() {
        let (release, wait) = mpsc::channel::<()>();
        let key = PathBuf::from("k-stuck");
        assert_eq!(
            probe(key.clone(), move || {
                let _ = wait.recv();
            }),
            None
        );
        // Still stuck: answered at once, no second thread.
        let t = std::time::Instant::now();
        assert_eq!(probe(key.clone(), || ()), None);
        assert!(t.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        // Released: the key is free again.
        for _ in 0..100 {
            if probe(key.clone(), || ()).is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the key stayed taken after the probe ended");
    }
}
