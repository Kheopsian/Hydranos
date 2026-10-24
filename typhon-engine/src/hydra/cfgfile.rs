//! Writing the configuration file: one writer at a time, and never half a file.
//!
//! Every edit of default.toml is read, changed and written back. Two edits
//! that interleave lose one of the changes, and a write that truncates the file
//! first leaves it empty or cut if the process dies in the middle -- the next
//! boot then cannot parse it. So the writers take [`lock`] around the whole
//! read-modify-write, and [`write_atomic`] puts the new text in a temporary
//! file beside the old one, flushes it to disk and renames it over.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static WRITERS: Mutex<()> = Mutex::new(());

/// The one-writer lock for the config file. Held from the read to the write
/// (and the reload that follows), never across an `.await`. A writer that
/// panicked leaves nothing half-done on disk, so a poisoned lock is still good.
pub fn lock() -> MutexGuard<'static, ()> {
    WRITERS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Replace the file at `path` with `contents`, all or nothing.
///
/// The file keeps the mode it has (an operator's 0600 stays 0600); a file that
/// does not exist yet is created 0600 when `private_files` is on. A symlink is
/// followed, so the link stays a link.
///
/// The text is written in place, as before, in only two cases: the temporary
/// file could not be CREATED beside the old one (the folder refuses new
/// files), or it could not be renamed over (a single file bind-mounted into a
/// container is busy for a rename). In both the original has not been touched.
/// A failure while writing or flushing the temporary (disk full, I/O error) is
/// returned as it is and the original stays as it was: an in-place write then
/// would truncate the real file and fail half way, which is what this exists to
/// prevent.
pub fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match replace(&target, contents) {
        Ok(()) => Ok(()),
        Err(Failed::Fatal(e)) => Err(e),
        Err(Failed::NoAtomic(e)) => {
            tracing::debug!(path = %target.display(), error = %e, "atomic config write unavailable; writing in place");
            std::fs::write(&target, contents)
        }
    }
}

/// What a refused change to `[mcp] torrent_roots` says.
pub const ROOTS_REFUSED: &str = "[mcp] torrent_roots is set in the config file only";

/// `write_atomic` for an edit made through a route of the API: refused (with
/// `PermissionDenied` and [`ROOTS_REFUSED`]) when the new text changes
/// `[mcp] torrent_roots` from what the file has now.
///
/// That list is the folders the agent endpoint may read a `.torrent` from, and
/// it is set in the config file by hand only: no route may write it, whatever
/// section and key a request names. Every route that edits the file writes
/// through here, so this is the one place that holds the rule.
pub fn write_edit(path: &Path, contents: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(old) = std::fs::read_to_string(&target) {
        if torrent_roots_changed(&old, contents) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                ROOTS_REFUSED,
            ));
        }
    }
    write_atomic(path, contents)
}

/// Whether `[mcp] torrent_roots` differs between two texts (absent and empty
/// are the same).
fn torrent_roots_changed(before: &str, after: &str) -> bool {
    fn roots(doc: &str) -> Option<Vec<String>> {
        let v: toml::Value = toml::from_str(doc).ok()?;
        let a = v.get("mcp")?.get("torrent_roots")?;
        let list: Vec<String> = match a.as_array() {
            Some(a) => a.iter().map(|x| x.to_string()).collect(),
            None => vec![a.to_string()],
        };
        (!list.is_empty()).then_some(list)
    }
    roots(before) != roots(after)
}

enum Failed {
    /// The original was not touched and the swap itself is not possible here:
    /// write in place.
    NoAtomic(std::io::Error),
    /// The new text did not reach the disk: stop, the original is intact.
    Fatal(std::io::Error),
}

/// Test seam: make the write of the temporary fail, on this thread only.
#[cfg(test)]
thread_local! {
    static FAIL_TEMP_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn temp_beside(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp-{}", std::process::id()));
    target.with_file_name(name)
}

fn replace(target: &Path, contents: &str) -> Result<(), Failed> {
    let tmp = temp_beside(target);
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    let (mut file, existing_mode) = {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let old = std::fs::metadata(target).ok();
        let existing = old.as_ref().map(|m| m.permissions().mode() & 0o7777);
        // Born private when the old file was or will be, so there is no
        // moment at a wider mode; a new file otherwise gets what
        // `fs::write` would give it (0666 under the umask).
        opts.mode(if existing.is_some() || crate::privfiles::enabled() {
            0o600
        } else {
            0o666
        });
        let file = opts.open(&tmp).map_err(Failed::NoAtomic)?;
        // The file keeps its owner, as an in-place write kept it: a rewrite by
        // root of the user's file must not leave it root's, unreadable to the
        // daemon at the next start. Where the owner cannot be given back (not
        // root, not the owner), the swap is not possible: write in place.
        if let Some(old) = &old {
            let mine = file.metadata().map_err(Failed::NoAtomic);
            let owner = (old.uid(), old.gid());
            let differs = mine.map(|m| (m.uid(), m.gid()) != owner);
            let moved = match differs {
                Ok(true) => std::os::unix::fs::fchown(&file, Some(owner.0), Some(owner.1))
                    .map_err(Failed::NoAtomic),
                Ok(false) => Ok(()),
                Err(e) => Err(e),
            };
            if let Err(e) = moved {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
        (file, existing)
    };
    #[cfg(not(unix))]
    let (mut file, existing_mode): (std::fs::File, Option<u32>) =
        (opts.open(&tmp).map_err(Failed::NoAtomic)?, None);

    let written = (|| {
        #[cfg(unix)]
        if let Some(mode) = existing_mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        let _ = existing_mode;
        #[cfg(test)]
        if FAIL_TEMP_WRITE.with(|c| c.get()) {
            return Err(std::io::Error::other("injected: no space left on device"));
        }
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    })();
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(Failed::Fatal(e));
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Failed::NoAtomic(e));
    }
    #[cfg(unix)]
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cfgfile-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_new_text_replaces_the_old_and_leaves_no_temporary() {
        let d = dir("replace");
        let p = d.join("default.toml");
        std::fs::write(&p, "old").unwrap();
        write_atomic(&p, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn the_mode_of_the_existing_file_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("mode");
        for mode in [0o600, 0o640, 0o644] {
            let p = d.join(format!("c{mode:o}.toml"));
            std::fs::write(&p, "old").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            write_atomic(&p, "new").unwrap();
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_stays_a_link_and_its_target_is_written() {
        let d = dir("link");
        let real = d.join("real.toml");
        let link = d.join("link.toml");
        std::fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, "new").unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A temporary that cannot be written (disk full) is an error, and the
    /// config as it was is still there, whole, with no temporary left.
    #[test]
    fn a_failed_write_of_the_temporary_leaves_the_original_whole() {
        let d = dir("full");
        let p = d.join("default.toml");
        std::fs::write(&p, "old").unwrap();
        FAIL_TEMP_WRITE.with(|c| c.set(true));
        let r = write_atomic(&p, "new");
        FAIL_TEMP_WRITE.with(|c| c.set(false));
        assert!(r.is_err(), "the failure is reported");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "old");
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The file keeps its owner: a rewrite by root (a `docker exec` without
    /// `-u`) of a file that belongs to the user the daemon runs as must not
    /// leave it root's. Only root can give a file to somebody else, so this
    /// runs where the tests run as root and says so where they do not.
    #[cfg(unix)]
    #[test]
    fn the_owner_of_the_file_is_kept() {
        use std::os::unix::fs::MetadataExt;
        let d = dir("owner");
        let p = d.join("default.toml");
        std::fs::write(&p, "old").unwrap();
        if std::os::unix::fs::chown(&p, Some(1000), Some(1000)).is_err() {
            eprintln!("not root: the owner cannot be changed here, test not run");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        write_atomic(&p, "new").unwrap();
        let m = std::fs::metadata(&p).unwrap();
        assert_eq!((m.uid(), m.gid()), (1000, 1000));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_file_is_created() {
        let d = dir("create");
        let p = d.join("fresh.toml");
        write_atomic(&p, "text").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "text");
        let _ = std::fs::remove_dir_all(&d);
    }
}
