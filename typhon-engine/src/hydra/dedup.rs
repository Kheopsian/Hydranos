//! Recognising data we already hold, under a name we have never seen.
//!
//! The `pieces` field of a torrent's info dict is the SHA-1 of the payload
//! stream, and the payload stream carries no names: not the file names, not
//! the directory they sit in, not the torrent's own. `info_hash` covers all
//! three. So two torrents that differ ONLY in naming have byte-identical
//! `pieces` and different `info_hash` -- which makes "do we already have this
//! content?" an exact lookup rather than a guess. No heuristic, no threshold,
//! no false positives.
//!
//! That is the whole idea. Two things follow from it and are worth stating
//! because both cost real terabytes when missed:
//!
//! 1. Identical `pieces` means the concatenated stream matches, so file *i* of
//!    one torrent is file *i* of the other, BY INDEX. There is nothing to
//!    match on names or sizes. The one caveat is that the same stream can in
//!    principle be cut into a different file list (100 MB as one file, or as
//!    two of 50) -- so the size list is compared before trusting the pairing.
//!
//! 2. A hardlink points at the same inode, so the bytes ARE the source's
//!    bytes (a reflink shares the blocks, which is the same thing until one
//!    side writes). If the source is complete and verified, the copy is complete by
//!    construction and rechecking it reads gigabytes to confirm a tautology.
//!
//! What this module deliberately does NOT do is partial matching. Measured on
//! the 300k-torrent production catalogue: 3 pairs above 50% shared pieces in
//! 20 000 torrents, and all three were genuinely different releases (two FLAC
//! rips, two APK builds, two epubs). The "same movie, different .nfo" case
//! that motivates partial matching does not occur here, and the machinery it
//! needs -- per-piece storage, offset alignment, a similarity threshold --
//! would be built to serve nothing.

use std::path::{Path, PathBuf};

/// One bencode value starting at `at`: the index just past it. A structural
/// walk (strings are skipped by their length prefix, never searched), so bytes
/// inside a string can never be mistaken for keys. Depth-bounded.
fn skip_value(b: &[u8], at: usize, depth: u32) -> Option<usize> {
    if depth > 64 {
        return None;
    }
    match *b.get(at)? {
        b'i' => Some(b[at..].iter().position(|&c| c == b'e')? + at + 1),
        b'l' | b'd' => {
            let mut i = at + 1;
            while *b.get(i)? != b'e' {
                i = skip_value(b, i, depth + 1)?;
            }
            Some(i + 1)
        }
        b'0'..=b'9' => str_at(b, at).map(|(_, end)| end),
        _ => None,
    }
}

/// The `<len>:<bytes>` string at `at`, with the index just past it.
fn str_at(b: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let colon = b.get(at..)?.iter().position(|&c| c == b':')? + at;
    let len: usize = std::str::from_utf8(&b[at..colon]).ok()?.parse().ok()?;
    let from = colon + 1;
    let end = from.checked_add(len)?;
    Some((b.get(from..end)?, end))
}

/// The raw bytes of the value stored under `key` at the TOP LEVEL of the
/// bencode dictionary `dict` (which must start with `d`).
fn dict_get<'a>(dict: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    if dict.first() != Some(&b'd') {
        return None;
    }
    let mut i = 1;
    while *dict.get(i)? != b'e' {
        let (k, after_key) = str_at(dict, i)?;
        let end = skip_value(dict, after_key, 0)?;
        if k == key {
            return Some(&dict[after_key..end]);
        }
        i = end;
    }
    None
}

/// The `info` dictionary of a torrent, found by structure: a lookalike key
/// inside any other string (a comment, say) is not the info dict.
fn info_dict(torrent: &[u8]) -> Option<&[u8]> {
    dict_get(torrent, b"info").filter(|v| v.first() == Some(&b'd'))
}

/// A bencode string value of the info dict.
fn bfield<'a>(blob: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let v = dict_get(info_dict(blob)?, key)?;
    str_at(v, 0).map(|(s, _)| s)
}

fn int_of(v: &[u8]) -> Option<u64> {
    let inner = v.strip_prefix(b"i")?.strip_suffix(b"e")?;
    std::str::from_utf8(inner).ok()?.parse().ok()
}

/// The raw `pieces` blob: the concatenated SHA-1 of every piece.
pub fn pieces(torrent_bytes: &[u8]) -> Option<&[u8]> {
    let p = bfield(torrent_bytes, b"pieces")?;
    (!p.is_empty() && p.len() % 20 == 0).then_some(p)
}

/// The content fingerprint: SHA-1 over the torrent's `pieces` blob.
///
/// SHA-1 is chosen because the engine already depends on it and this is an
/// INDEX, not a decision: a lookup narrows the catalogue to a handful of rows,
/// and `same_content` then compares the actual bytes before anything is
/// linked. A forged SHA-1 collision therefore buys an attacker a wasted
/// lookup, not a torrent seeded from the wrong data.
///
/// Returns None for a torrent with no `pieces` at all (a v2-only torrent, or a
/// blob that is not a torrent) -- those simply never match anything, which is
/// the right answer rather than an error.
pub fn content_key(torrent_bytes: &[u8]) -> Option<String> {
    Some(sha1_hex(pieces(torrent_bytes)?))
}

/// Whether two torrents describe the same payload stream, compared exactly.
///
/// This is the check that actually authorises a link. `content_key` only picks
/// the candidates out of 300k rows.
pub fn same_content(a: &[u8], b: &[u8]) -> bool {
    match (pieces(a), pieces(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(data);
    typhon_engine::torrent::hex_encode_bytes(&h.finalize())
}

/// One file of a torrent, as the payload stream orders them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the torrent root, already joined with `/`.
    pub rel: String,
    pub len: u64,
}

/// The file list, in stream order, with the root directory if there is one.
///
/// `name` is the root directory for a multi-file torrent and the file itself
/// for a single-file one -- the same distinction the engine makes when it
/// builds an on-disk path.
#[derive(Debug, Clone)]
pub struct Layout {
    pub name: String,
    pub multi_file: bool,
    pub files: Vec<Entry>,
}

impl Layout {
    #[cfg(test)]
    pub fn total_len(&self) -> u64 {
        self.files.iter().map(|f| f.len).sum()
    }

    /// Where file `i` lives under `save_path`, or None when the torrent names
    /// a place outside it: every component of the name and of the file path
    /// must be a plain name (not empty, `.`, `..`, absolute, or holding a NUL).
    pub fn on_disk(&self, save_path: &str, i: usize) -> Option<PathBuf> {
        let f = self.files.get(i)?;
        let base = Path::new(save_path);
        if !plain_rel(&self.name) {
            return None;
        }
        Some(if self.multi_file {
            if !plain_rel(&f.rel) {
                return None;
            }
            base.join(&self.name).join(&f.rel)
        } else {
            base.join(&self.name)
        })
    }
}

/// A relative path made only of ordinary components.
fn plain_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\0')
        && rel
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Read the file list out of raw torrent bytes.
///
/// A structural walk of the `info` dict only (never a substring search of the
/// whole blob), so it reads what the validated metainfo says and nothing an
/// outer field could impersonate.
pub fn layout(torrent_bytes: &[u8]) -> Option<Layout> {
    let info = info_dict(torrent_bytes)?;
    let name = String::from_utf8_lossy(bfield(torrent_bytes, b"name")?).into_owned();

    // Single-file torrents have `length` in the info dict and no `files` key.
    let Some(files_v) = dict_get(info, b"files") else {
        let len = int_of(dict_get(info, b"length")?)?;
        return Some(Layout {
            name,
            multi_file: false,
            files: vec![Entry {
                rel: String::new(),
                len,
            }],
        });
    };
    if files_v.first() != Some(&b'l') {
        return None;
    }

    let mut files = Vec::new();
    let mut i = 1;
    while *files_v.get(i)? != b'e' {
        let end = skip_value(files_v, i, 0)?;
        let entry = &files_v[i..end];
        let len = int_of(dict_get(entry, b"length")?)?;
        let path = dict_get(entry, b"path")?;
        if path.first() != Some(&b'l') {
            return None;
        }
        let mut comps = Vec::new();
        let mut j = 1;
        while *path.get(j)? != b'e' {
            let (c, next) = str_at(path, j)?;
            comps.push(String::from_utf8_lossy(c).into_owned());
            j = next;
        }
        files.push(Entry {
            rel: comps.join("/"),
            len,
        });
        i = end;
    }

    if files.is_empty() {
        return None;
    }
    Some(Layout {
        name,
        multi_file: true,
        files,
    })
}

/// A torrent we already hold whose payload is the incoming one's payload.
#[derive(Debug, Clone)]
// No caller today. Kept, not deleted: clearing lints is not the moment to drop code.
#[allow(dead_code)]
pub struct Source {
    pub info_hash: String,
    pub session: String,
    pub save_path: String,
    pub name: String,
}

/// One file to create, and the file whose inode it will share.
#[derive(Debug, Clone)]
pub struct Link {
    pub from: PathBuf,
    pub to: PathBuf,
    pub len: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub links: Vec<Link>,
    /// Set when source and destination sit on different filesystems, where
    /// `link()` returns EXDEV and no amount of retrying helps.
    pub cross_device: bool,
    pub bytes: u64,
}

/// Why a match cannot be turned into links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Same stream, different file boundaries. Vanishingly rare, and pairing
    /// by index here would link the wrong bytes together.
    LayoutMismatch,
    /// A source file is missing or the wrong size on disk. The store says we
    /// hold this data; the disk disagrees, and the disk wins.
    SourceIncomplete(String),
    /// The destination already holds a file, so linking would mean deciding
    /// what to destroy. That is not this code's call to make.
    DestinationOccupied(String),
}

/// Work out the links that would make `want` seedable from `have`'s data.
///
/// Nothing is created and nothing is removed: this only reads. The separation
/// is deliberate -- the caller can show the plan, log it, or diff it against a
/// dry run before anything touches the filesystem.
pub fn plan(
    have: &Layout,
    have_save_path: &str,
    want: &Layout,
    want_save_path: &str,
) -> Result<Plan, Refusal> {
    // Identical `pieces` guarantees the same byte stream, not the same cut
    // through it. Comparing the size list is what makes pairing by index safe.
    if have.files.len() != want.files.len()
        || have
            .files
            .iter()
            .zip(&want.files)
            .any(|(a, b)| a.len != b.len)
    {
        return Err(Refusal::LayoutMismatch);
    }

    let mut out = Plan::default();
    for i in 0..want.files.len() {
        let (Some(src), Some(dst)) = (
            have.on_disk(have_save_path, i),
            want.on_disk(want_save_path, i),
        ) else {
            return Err(Refusal::LayoutMismatch);
        };

        let md = std::fs::metadata(&src)
            .map_err(|_| Refusal::SourceIncomplete(src.to_string_lossy().into_owned()))?;
        if md.len() != have.files[i].len {
            return Err(Refusal::SourceIncomplete(
                src.to_string_lossy().into_owned(),
            ));
        }
        if dst.exists() {
            return Err(Refusal::DestinationOccupied(
                dst.to_string_lossy().into_owned(),
            ));
        }

        out.bytes += have.files[i].len;
        out.links.push(Link {
            from: src,
            to: dst,
            len: have.files[i].len,
        });
    }

    out.cross_device = out
        .links
        .first()
        .map(|l| !same_filesystem(&l.from, &l.to))
        .unwrap_or(false);
    Ok(out)
}

/// Whether two paths can be hardlinked, i.e. sit on one filesystem.
///
/// Compared on the device of the nearest EXISTING ancestor, because the
/// destination directory has not been created yet at planning time.
fn same_filesystem(a: &Path, b: &Path) -> bool {
    match (device_of(a), device_of(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn device_of(p: &Path) -> Option<u64> {
    crate::platform::volume_id_nearest(p)
}

/// How a planned file is made to exist under its new name (`[dedup] method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// A second directory entry for the same inode. Works on every local
    /// filesystem; the two names share one file, link count and all.
    #[default]
    Hardlink,
    /// A copy-on-write clone (`FICLONE`): a new inode sharing the source's
    /// blocks until either side writes. Only where the filesystem can clone
    /// (btrfs, XFS with reflink, ZFS >= 2.2 with block cloning). On ZFS the
    /// clone can wait for a txg sync, and that wait cannot be interrupted or
    /// timed out from here. A refused clone is
    /// NOT retried as a hardlink: whoever picked this asked for separate
    /// files, and a hardlink would quietly give them one.
    Reflink,
}

#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub created: usize,
    pub bytes: u64,
}

/// Create the planned links.
///
/// Directories are CREATED, never linked: `link()` on a directory is refused
/// by the kernel, so a renamed folder means a new folder holding links to the
/// same file inodes. On failure everything created here is removed again --
/// a half-linked torrent seeds corrupt data to the swarm.
pub fn apply(p: &Plan, method: Method) -> std::io::Result<Applied> {
    let mut done: Vec<PathBuf> = Vec::with_capacity(p.links.len());
    let mut out = Applied::default();

    for link in &p.links {
        if let Some(parent) = link.to.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                unwind(&done);
                return Err(e);
            }
        }
        let made = match method {
            Method::Hardlink => std::fs::hard_link(&link.from, &link.to),
            Method::Reflink => reflink(&link.from, &link.to),
        };
        if let Err(e) = made {
            unwind(&done);
            return Err(e);
        }
        done.push(link.to.clone());
        out.created += 1;
        out.bytes += link.len;
    }
    Ok(out)
}

/// Clone `from` into a new file `to`, or fail without leaving `to` behind.
///
/// `FICLONE` and not `copy_file_range`: the latter falls back to copying the
/// bytes when it cannot share them, which would turn a dedup into a second
/// copy of the payload without a word.
#[cfg(target_os = "linux")]
fn reflink(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // _IOW(0x94, 9, int), the same on every Linux architecture.
    const FICLONE: libc::c_ulong = 0x4004_9409;

    let src = std::fs::File::open(from)?;
    // create_new: never clone over a file someone else just put there.
    let dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    // SAFETY: both descriptors are open for the duration of the call, and
    // FICLONE reads no memory from us beyond the integer argument.
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        drop(dst);
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn reflink(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "reflink is only implemented on Linux",
    ))
}

fn unwind(created: &[PathBuf]) {
    for p in created.iter().rev() {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal multi-file torrent: files with the given names/sizes.
    fn multi(name: &str, files: &[(&str, u64)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"d4:infod5:filesl");
        for (path, len) in files {
            v.extend_from_slice(
                format!("d6:lengthi{len}e4:pathl{}:{path}ee", path.len()).as_bytes(),
            );
        }
        v.extend_from_slice(format!("e4:name{}:{name}", name.len()).as_bytes());
        v.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        v.extend_from_slice(&[7u8; 20]);
        v.extend_from_slice(b"ee");
        v
    }

    fn single(name: &str, len: u64, piece: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(
            format!("d4:infod6:lengthi{len}e4:name{}:{name}", name.len()).as_bytes(),
        );
        v.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        v.extend_from_slice(&[piece; 20]);
        v.extend_from_slice(b"ee");
        v
    }

    #[test]
    fn renaming_does_not_change_the_content_key() {
        // The whole premise: different names, same payload, same key.
        let a = single("merde.mkv", 1234, 3);
        let b = single("Film.2024.1080p.mkv", 1234, 3);
        assert_ne!(a, b);
        assert_eq!(content_key(&a), content_key(&b));
        assert!(content_key(&a).is_some());
    }

    #[test]
    fn different_payload_gives_a_different_key() {
        let a = single("x.mkv", 1234, 3);
        let b = single("x.mkv", 1234, 4);
        assert_ne!(content_key(&a), content_key(&b));
    }

    #[test]
    fn piece_length_is_not_mistaken_for_length() {
        // `12:piece lengthi16384e` contains "lengthi"; only `6:lengthi` counts.
        let t = single("x.mkv", 999, 1);
        let l = layout(&t).unwrap();
        assert_eq!(l.total_len(), 999);
    }

    #[test]
    fn multi_file_layout_keeps_stream_order() {
        let t = multi("Pack", &[("a.mkv", 10), ("b.nfo", 20)]);
        let l = layout(&t).unwrap();
        assert!(l.multi_file);
        assert_eq!(l.name, "Pack");
        assert_eq!(l.files.len(), 2);
        assert_eq!(
            l.files[0],
            Entry {
                rel: "a.mkv".into(),
                len: 10
            }
        );
        assert_eq!(
            l.files[1],
            Entry {
                rel: "b.nfo".into(),
                len: 20
            }
        );
    }

    #[test]
    fn on_disk_puts_multi_file_under_the_root() {
        let t = multi("Pack", &[("a.mkv", 10)]);
        let l = layout(&t).unwrap();
        assert_eq!(
            l.on_disk("/data", 0).unwrap(),
            PathBuf::from("/data/Pack/a.mkv")
        );

        let s = layout(&single("f.mkv", 10, 1)).unwrap();
        assert_eq!(s.on_disk("/data", 0).unwrap(), PathBuf::from("/data/f.mkv"));
    }

    #[test]
    fn a_different_cut_of_the_same_stream_is_refused() {
        // Same total bytes, different boundaries: pairing by index here would
        // link the wrong file to the wrong name.
        let have = layout(&multi("A", &[("one", 100)])).unwrap();
        let want = layout(&multi("B", &[("x", 50), ("y", 50)])).unwrap();
        assert_eq!(
            plan(&have, "/a", &want, "/b").unwrap_err(),
            Refusal::LayoutMismatch
        );
    }

    #[test]
    fn plans_and_links_a_renamed_folder() {
        let dir = std::env::temp_dir().join(format!("dedup-test-{}", std::process::id()));
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(src.join("Old.Name")).unwrap();
        std::fs::write(src.join("Old.Name").join("a.mkv"), b"0123456789").unwrap();

        let have = layout(&multi("Old.Name", &[("a.mkv", 10)])).unwrap();
        let want = layout(&multi("New.Name", &[("a.mkv", 10)])).unwrap();

        let p = plan(&have, src.to_str().unwrap(), &want, dst.to_str().unwrap()).unwrap();
        assert_eq!(p.links.len(), 1);
        assert_eq!(p.bytes, 10);

        let done = apply(&p, Method::Hardlink).unwrap();
        assert_eq!(done.created, 1);

        // The link must BE the source inode, not a copy of it. Unix only:
        // NTFS has a file id and a link count too, but reading them needs an
        // open handle rather than metadata, and what this test guards -- that
        // apply() linked instead of copying -- is asserted by `created` above
        // on every platform.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = std::fs::metadata(src.join("Old.Name").join("a.mkv")).unwrap();
            let b = std::fs::metadata(dst.join("New.Name").join("a.mkv")).unwrap();
            assert_eq!(a.ino(), b.ino());
            assert_eq!(b.nlink(), 2);
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_source_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("dedup-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let have = layout(&multi("Gone", &[("a.mkv", 10)])).unwrap();
        let want = layout(&multi("New", &[("a.mkv", 10)])).unwrap();
        assert!(matches!(
            plan(&have, dir.to_str().unwrap(), &want, dir.to_str().unwrap()),
            Err(Refusal::SourceIncomplete(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_truncated_source_is_refused() {
        // The store says we hold it; the disk says otherwise. Disk wins.
        let dir = std::env::temp_dir().join(format!("dedup-short-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("P")).unwrap();
        std::fs::write(dir.join("P").join("a.mkv"), b"short").unwrap();
        let have = layout(&multi("P", &[("a.mkv", 10)])).unwrap();
        let want = layout(&multi("Q", &[("a.mkv", 10)])).unwrap();
        assert!(matches!(
            plan(&have, dir.to_str().unwrap(), &want, dir.to_str().unwrap()),
            Err(Refusal::SourceIncomplete(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_leaves_nothing_behind_when_a_link_fails() {
        // Two files, the second already taken: the first must not survive, or
        // the torrent half-seeds.
        let dir = std::env::temp_dir().join(format!("dedup-unwind-{}", std::process::id()));
        let src = dir.join("s");
        let dst = dir.join("d");
        std::fs::create_dir_all(src.join("P")).unwrap();
        std::fs::create_dir_all(dst.join("P")).unwrap();
        std::fs::write(src.join("P").join("a"), b"aaaa").unwrap();
        std::fs::write(src.join("P").join("b"), b"bbbb").unwrap();
        std::fs::write(dst.join("P").join("b"), b"taken").unwrap();

        let p = Plan {
            links: vec![
                Link {
                    from: src.join("P").join("a"),
                    to: dst.join("P").join("a"),
                    len: 4,
                },
                Link {
                    from: src.join("P").join("b"),
                    to: dst.join("P").join("b"),
                    len: 4,
                },
            ],
            cross_device: false,
            bytes: 8,
        };
        assert!(apply(&p, Method::Hardlink).is_err());
        assert!(
            !dst.join("P").join("a").exists(),
            "first link was left behind"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reflink_gives_a_separate_file_or_nothing() {
        // Reflink is only possible where the filesystem clones, and the test
        // directory may sit on one that cannot (tmpfs). Either outcome is
        // allowed; a hardlink in its place is not.
        let dir = std::env::temp_dir().join(format!("dedup-reflink-{}", std::process::id()));
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(src.join("Old")).unwrap();
        std::fs::write(src.join("Old").join("a.mkv"), b"0123456789").unwrap();

        let have = layout(&multi("Old", &[("a.mkv", 10)])).unwrap();
        let want = layout(&multi("New", &[("a.mkv", 10)])).unwrap();
        let p = plan(&have, src.to_str().unwrap(), &want, dst.to_str().unwrap()).unwrap();

        let made = dst.join("New").join("a.mkv");
        match apply(&p, Method::Reflink) {
            Ok(done) => {
                assert_eq!(done.created, 1);
                assert_eq!(std::fs::read(&made).unwrap(), b"0123456789");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let a = std::fs::metadata(src.join("Old").join("a.mkv")).unwrap();
                    let b = std::fs::metadata(&made).unwrap();
                    assert_ne!(a.ino(), b.ino(), "reflink produced a hardlink");
                    assert_eq!(a.nlink(), 1);
                }
            }
            Err(e) => {
                // Set where TMPDIR is known to clone, so a run there cannot
                // pass on the refusal branch alone.
                assert!(
                    std::env::var_os("DEDUP_TEST_MUST_CLONE").is_none(),
                    "clone refused: {e}"
                );
                assert!(!made.exists(), "a refused clone left a file behind");
            }
        }
        // Whatever happened, the source still has exactly one name.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = std::fs::metadata(src.join("Old").join("a.mkv")).unwrap();
            assert_eq!(a.nlink(), 1, "the source was hardlinked");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn method_reads_from_the_config_words() {
        #[derive(serde::Deserialize)]
        struct T {
            m: Method,
        }
        let r: T = toml::from_str("m = \"reflink\"").unwrap();
        assert_eq!(r.m, Method::Reflink);
        let h: T = toml::from_str("m = \"hardlink\"").unwrap();
        assert_eq!(h.m, Method::Hardlink);
        assert!(toml::from_str::<T>("m = \"copy\"").is_err());
        assert_eq!(Method::default(), Method::Hardlink);
    }

    #[test]
    fn a_torrent_without_pieces_has_no_key() {
        assert_eq!(content_key(b"d4:infod4:name1:xee"), None);
    }

    #[test]
    fn same_content_compares_bytes_not_the_index() {
        // What actually authorises a link: the pieces themselves, so that a
        // collision in the index cannot promote a wrong candidate.
        let a = single("one.mkv", 10, 3);
        let b = single("two.mkv", 10, 3);
        let c = single("two.mkv", 10, 9);
        assert!(same_content(&a, &b));
        assert!(!same_content(&a, &c));
        assert!(!same_content(&a, b"not a torrent"));
    }

    /// An outer string that carries `name`/`pieces` lookalikes, in front of `info`.
    fn with_comment(torrent: &[u8], comment: &[u8]) -> Vec<u8> {
        assert!(torrent.starts_with(b"d4:info"));
        let mut v = format!("d7:comment{}:", comment.len()).into_bytes();
        v.extend_from_slice(comment);
        v.extend_from_slice(&torrent[1..]);
        v
    }

    fn bstr(key: &str, val: &[u8]) -> Vec<u8> {
        let mut v = format!("{}:{key}{}:", key.len(), val.len()).into_bytes();
        v.extend_from_slice(val);
        v
    }

    #[test]
    fn lookalike_fields_outside_the_info_dict_are_not_read() {
        let real = single("b", 100, 1);
        let mut comment = bstr("name", b"/etc/planted");
        comment.extend(bstr("pieces", &[9u8; 20]));
        let forged = with_comment(&real, &comment);
        assert_eq!(pieces(&forged), pieces(&real), "pieces come from info only");
        assert_eq!(content_key(&forged), content_key(&real));
        assert_eq!(
            layout(&forged).unwrap().name,
            "b",
            "name comes from info only"
        );
        assert!(!same_content(&forged, &single("c", 100, 9)));
    }

    #[test]
    fn a_name_or_path_that_leaves_the_save_root_has_no_location() {
        for bad in ["/etc/planted", "../climbed", "a/../../b", "..", ""] {
            let l = layout(&single(bad, 10, 1)).unwrap();
            assert!(l.on_disk("/save", 0).is_none(), "single {bad:?}");
            let l = layout(&multi(bad, &[("f", 10)])).unwrap();
            assert!(l.on_disk("/save", 0).is_none(), "root {bad:?}");
        }
        for bad in ["/etc/passwd", "..", "a/../../x"] {
            let l = layout(&multi("ok", &[(bad, 10)])).unwrap();
            assert!(l.on_disk("/save", 0).is_none(), "file {bad:?}");
        }
        let l = layout(&multi("ok", &[("sub/f", 10)])).unwrap();
        assert_eq!(l.on_disk("/save", 0), Some(PathBuf::from("/save/ok/sub/f")));
    }

    #[test]
    fn a_plan_never_links_to_or_from_outside_the_save_root() {
        let td = std::env::temp_dir().join(format!("dedup-confine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&td);
        let (lib, dl) = (td.join("lib"), td.join("dl"));
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        std::fs::write(lib.join("victim.bin"), [1u8; 100]).unwrap();
        let have = layout(&single("victim.bin", 100, 1)).unwrap();
        let outside = td.join("planted.bin");
        for name in [
            outside.to_string_lossy().into_owned(),
            "../planted.bin".to_string(),
        ] {
            let want = layout(&single(&name, 100, 1)).unwrap();
            let r = plan(&have, &lib.to_string_lossy(), &want, &dl.to_string_lossy());
            assert!(r.is_err(), "{name:?} must be refused");
        }
        assert!(!outside.exists());
        let _ = std::fs::remove_dir_all(&td);
    }
}
