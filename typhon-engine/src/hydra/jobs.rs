//! Long jobs that outlive the request that asked for them.
//!
//! Moving a torrent's data can take hours. The request that starts it returns
//! at once with a job id, the work happens on a tokio task, and the row in the
//! `jobs` table is what survives a restart -- so a move interrupted by a deploy
//! is resumed rather than left half-done, with the data split across two disks
//! and neither copy complete. The restart queues it again (`jobsrun::spawn`),
//! and a graduation or move that had already taken its torrent out of the
//! engine finishes from the record carried in the job's params.

use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    /// Tests run on one filesystem: this makes every move a copy.
    pub(crate) static FORCE_COPY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A copy on this thread fails with "no space left" once it has written
    /// this many bytes, leaving what it wrote: a full disk.
    pub(crate) static COPY_FULL_AFTER: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    /// The free space `free_space_near` reports on this thread.
    pub(crate) static FREE_SPACE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// A move, planned before anything is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Same filesystem: a rename, which is instant and atomic.
    Rename,
    /// Different filesystems: copy every byte, then unlink the source.
    CopyThenDelete { bytes: u64 },
    /// Nothing to do.
    AlreadyThere,
}

/// Decide how a move would be done, without doing it.
///
/// A rename across filesystems fails with EXDEV; asking first is what lets the
/// caller know whether this is instant or an hour of disk.
pub fn plan(source: &Path, target: &Path) -> std::io::Result<Plan> {
    if source == target {
        return Ok(Plan::AlreadyThere);
    }
    let meta = std::fs::metadata(source)?;
    #[cfg(test)]
    if FORCE_COPY.with(|c| c.get()) {
        return Ok(Plan::CopyThenDelete { bytes: meta.len() });
    }
    if same_filesystem(source, target) {
        return Ok(Plan::Rename);
    }
    Ok(Plan::CopyThenDelete { bytes: meta.len() })
}

/// Whether two paths live on the same device.
///
/// The target usually does not exist yet, so its nearest existing ancestor is
/// what gets asked -- that is the filesystem the file would land on.
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    match (device_of(a), device_of_nearest(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn device_of(p: &Path) -> Option<u64> {
    crate::platform::volume_id(p)
}

fn device_of_nearest(p: &Path) -> Option<u64> {
    let mut cur = p;
    loop {
        if let Some(dev) = device_of(cur) {
            return Some(dev);
        }
        cur = cur.parent()?;
    }
}

/// How many names this file has on disk.
///
/// A file with more than one is hardlinked into the media library. Copying and
/// unlinking it would break that link and double the space it takes: the
/// library's copy stops being the same bytes. A move like that has to be a
/// rename or nothing.
pub fn link_count(p: &Path) -> u64 {
    crate::platform::link_count(p)
}

/// Free bytes on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    crate::platform::free_space(path)
}

/// Copy one file, leaving the source where it is.
///
/// ⚠ Deliberately a read/write loop and NOT `std::fs::copy`. On Linux that
/// calls `copy_file_range`, which ZFS turns into block cloning -- and block
/// cloning on this pool wedges the calling process in uninterruptible sleep.
/// It has already frozen builds on this machine for 45 minutes at 0% CPU.
///
/// The copy is flushed before this returns: a caller that deletes the source
/// next must not be deleting the only complete copy.
///
/// ⭐ A copy that fails (a full disk above all) does not leave its partial
/// file: it would sit on the destination, fill the disk a little more, and
/// make every retry meet "a file is already there".
pub fn copy_only(source: &Path, target: &Path) -> std::io::Result<u64> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut src = std::fs::File::open(source)?;
    let mut dst = std::fs::File::create(target)?;
    match copy_loop(&mut src, &mut dst) {
        Ok(copied) => Ok(copied),
        Err(e) => {
            drop(dst);
            let _ = std::fs::remove_file(target);
            Err(e)
        }
    }
}

fn copy_loop(src: &mut std::fs::File, dst: &mut std::fs::File) -> std::io::Result<u64> {
    use std::io::{Read, Write};

    let mut buf = vec![0u8; 4 << 20];
    let mut copied = 0u64;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        #[cfg(test)]
        if let Some(limit) = COPY_FULL_AFTER.with(|c| c.get()) {
            let room = limit.saturating_sub(copied) as usize;
            if room < n {
                dst.write_all(&buf[..room])?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "no space left on device",
                ));
            }
        }
        dst.write_all(&buf[..n])?;
        copied += n as u64;
    }
    dst.sync_all()?;
    Ok(copied)
}

/// Copy one file, then remove the source.
///
/// The source is removed only after the copy is complete and flushed: a
/// half-copied file with its source already gone is data lost.
pub fn copy_then_delete(source: &Path, target: &Path) -> std::io::Result<u64> {
    let copied = copy_only(source, target)?;
    std::fs::remove_file(source)?;
    Ok(copied)
}

/// Run one move, breaking a hardlink only if the operator said so.
///
/// Breaking one is not data loss -- the other name keeps its bytes -- but it
/// doubles the space those bytes take, which is why it is asked and never
/// assumed.
pub fn run_move_allowing(
    source: &Path,
    target: &Path,
    allow_breaking_hardlinks: bool,
) -> std::io::Result<Plan> {
    let plan = plan(source, target)?;
    match plan {
        Plan::AlreadyThere => Ok(plan),
        Plan::Rename => {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(source, target)?;
            Ok(plan)
        }
        Plan::CopyThenDelete { bytes } => {
            // Refuse rather than fill the target disk and fail halfway, which
            // leaves the source deleted for the files already done.
            match free_space_near(target) {
                Some(free) if free < bytes => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::StorageFull,
                        format!("{bytes} bytes to move, {free} free on the target"),
                    ));
                }
                _ => {}
            }
            if !allow_breaking_hardlinks && link_count(source) > 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "this file is hardlinked elsewhere: copying it across a filesystem \
                     would break the link and double the space it takes",
                ));
            }
            copy_then_delete(source, target)?;
            Ok(plan)
        }
    }
}

/// Free bytes where `path` would land. The target usually does not exist yet,
/// so its nearest existing ancestor is what gets asked.
pub fn free_space_near(path: &Path) -> Option<u64> {
    #[cfg(test)]
    if let Some(free) = FREE_SPACE.with(|c| c.get()) {
        return Some(free);
    }
    let mut cur = path;
    loop {
        if cur.exists() {
            return free_space(cur);
        }
        cur = cur.parent()?;
    }
}

/// Bytes that moving these (from, to) pairs would copy: what a move across
/// filesystems writes, a rename writing nothing. A file already at its
/// destination is overwritten, so its size is not counted twice.
pub fn bytes_to_copy(files: &[(PathBuf, PathBuf)]) -> u64 {
    files
        .iter()
        .filter_map(|(from, to)| match plan(from, to) {
            Ok(Plan::CopyThenDelete { bytes }) => {
                let there = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
                Some(bytes.saturating_sub(there))
            }
            _ => None,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_onto_itself_does_nothing() {
        let dir = std::env::temp_dir().join("hydra-jobs-test-same");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(plan(&f, &f).unwrap(), Plan::AlreadyThere);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_paths_under_one_filesystem_are_a_rename() {
        let dir = std::env::temp_dir().join("hydra-jobs-test-rename");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a");
        let dst = dir.join("b");
        std::fs::write(&src, b"hello").unwrap();
        assert_eq!(plan(&src, &dst).unwrap(), Plan::Rename);
        run_move_allowing(&src, &dst, false).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
        assert!(!src.exists(), "the source is gone after a rename");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A hardlinked file is in the media library too. Copying it across a
    /// filesystem breaks that link: the library keeps the old bytes and the
    /// space is paid twice.
    #[test]
    fn a_hardlinked_file_is_counted_as_such() {
        let dir = std::env::temp_dir().join("hydra-jobs-test-link");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::write(&a, b"x").unwrap();
        assert_eq!(link_count(&a), 1);
        std::fs::hard_link(&a, &b).unwrap();
        assert_eq!(link_count(&a), 2, "the file now has two names");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_only_leaves_the_source() {
        let dir = std::env::temp_dir().join("hydra-jobs-test-copyonly");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        let b = dir.join("sub").join("b");
        std::fs::write(&a, b"payload").unwrap();
        assert_eq!(copy_only(&a, &b).unwrap(), 7);
        assert_eq!(std::fs::read(&b).unwrap(), b"payload");
        assert!(a.exists(), "a copy must not remove its source");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn free_space_is_asked_of_the_nearest_existing_ancestor() {
        let dir = std::env::temp_dir().join("hydra-jobs-test-free");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(free_space_near(&dir.join("x").join("y")).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_nearest_existing_ancestor_decides_the_target_filesystem() {
        let dir = std::env::temp_dir().join("hydra-jobs-test-dev");
        std::fs::create_dir_all(&dir).unwrap();
        // The target does not exist yet, which is the normal case.
        let target = dir.join("not-created-yet").join("file");
        assert!(same_filesystem(&dir, &target));
        std::fs::remove_dir_all(&dir).ok();
    }
}
