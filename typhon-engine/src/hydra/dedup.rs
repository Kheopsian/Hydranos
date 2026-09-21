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
//!    bytes. If the source is complete and verified, the copy is complete by
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

/// A bencode string value read straight out of the raw torrent bytes.
///
/// Parsing the whole dict to reach two fields would cost an allocation per
/// torrent on a path that runs 300k times at boot. The key is matched with its
/// bencode length prefix (`6:pieces`), which is why `12:piece length` cannot
/// collide with it.
fn bfield<'a>(blob: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut needle = Vec::with_capacity(key.len() + 4);
    needle.extend_from_slice(key.len().to_string().as_bytes());
    needle.push(b':');
    needle.extend_from_slice(key);

    let at = blob
        .windows(needle.len())
        .position(|w| w == needle.as_slice())?;
    let rest = &blob[at + needle.len()..];
    let colon = rest.iter().position(|&b| b == b':')?;
    let len: usize = std::str::from_utf8(&rest[..colon]).ok()?.parse().ok()?;
    let from = colon + 1;
    rest.get(from..from + len)
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
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
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
    pub fn total_len(&self) -> u64 {
        self.files.iter().map(|f| f.len).sum()
    }

    /// Where file `i` lives under `save_path`.
    pub fn on_disk(&self, save_path: &str, i: usize) -> Option<PathBuf> {
        let f = self.files.get(i)?;
        let base = Path::new(save_path);
        Some(if self.multi_file {
            base.join(&self.name).join(&f.rel)
        } else {
            base.join(&self.name)
        })
    }
}

/// Read the file list out of raw torrent bytes.
///
/// A hand-rolled walk of the `info` dict rather than a full bencode parse: the
/// engine already has a parser, but it lives behind `TorrentMeta` and pulling
/// a whole meta in to read two fields is what makes a boot-time pass slow.
pub fn layout(torrent_bytes: &[u8]) -> Option<Layout> {
    let name = String::from_utf8_lossy(bfield(torrent_bytes, b"name")?).into_owned();

    // Single-file torrents have `length` in the info dict and no `files` key.
    let files_at = find_key(torrent_bytes, b"5:filesl");
    let Some(mut cursor) = files_at else {
        let len = find_int(torrent_bytes, b"6:lengthi")?;
        return Some(Layout {
            name,
            multi_file: false,
            files: vec![Entry { rel: String::new(), len }],
        });
    };

    let mut files = Vec::new();
    // Each entry is a dict holding `length` and `path` (a list of components).
    while let Some(next) = find_from(torrent_bytes, cursor, b"6:lengthi") {
        let Some(len) = read_int_at(torrent_bytes, next + b"6:lengthi".len()) else {
            break;
        };
        let Some(path_at) = find_from(torrent_bytes, next, b"4:pathl") else {
            break;
        };
        let (comps, end) = read_string_list(torrent_bytes, path_at + b"4:pathl".len())?;
        files.push(Entry { rel: comps.join("/"), len });
        cursor = end;
    }

    if files.is_empty() {
        return None;
    }
    Some(Layout { name, multi_file: true, files })
}

fn find_key(blob: &[u8], needle: &[u8]) -> Option<usize> {
    find_from(blob, 0, needle)
}

fn find_from(blob: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= blob.len() {
        return None;
    }
    blob[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn find_int(blob: &[u8], needle: &[u8]) -> Option<u64> {
    let at = find_key(blob, needle)?;
    read_int_at(blob, at + needle.len())
}

fn read_int_at(blob: &[u8], at: usize) -> Option<u64> {
    let end = blob[at..].iter().position(|&b| b == b'e')? + at;
    std::str::from_utf8(&blob[at..end]).ok()?.parse().ok()
}

/// Read `<len>:<bytes>` items until the list's `e`.
fn read_string_list(blob: &[u8], mut at: usize) -> Option<(Vec<String>, usize)> {
    let mut out = Vec::new();
    loop {
        match blob.get(at)? {
            b'e' => return Some((out, at + 1)),
            _ => {
                let colon = blob[at..].iter().position(|&b| b == b':')? + at;
                let len: usize = std::str::from_utf8(&blob[at..colon]).ok()?.parse().ok()?;
                let from = colon + 1;
                out.push(String::from_utf8_lossy(blob.get(from..from + len)?).into_owned());
                at = from + len;
            }
        }
    }
}

/// A torrent we already hold whose payload is the incoming one's payload.
#[derive(Debug, Clone)]
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
        || have.files.iter().zip(&want.files).any(|(a, b)| a.len != b.len)
    {
        return Err(Refusal::LayoutMismatch);
    }

    let mut out = Plan::default();
    for i in 0..want.files.len() {
        let (Some(src), Some(dst)) =
            (have.on_disk(have_save_path, i), want.on_disk(want_save_path, i))
        else {
            return Err(Refusal::LayoutMismatch);
        };

        let md = std::fs::metadata(&src)
            .map_err(|_| Refusal::SourceIncomplete(src.to_string_lossy().into_owned()))?;
        if md.len() != have.files[i].len {
            return Err(Refusal::SourceIncomplete(src.to_string_lossy().into_owned()));
        }
        if dst.exists() {
            return Err(Refusal::DestinationOccupied(dst.to_string_lossy().into_owned()));
        }

        out.bytes += have.files[i].len;
        out.links.push(Link { from: src, to: dst, len: have.files[i].len });
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
pub fn apply(p: &Plan) -> std::io::Result<Applied> {
    let mut done: Vec<PathBuf> = Vec::with_capacity(p.links.len());
    let mut out = Applied::default();

    for link in &p.links {
        if let Some(parent) = link.to.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                unwind(&done);
                return Err(e);
            }
        }
        if let Err(e) = std::fs::hard_link(&link.from, &link.to) {
            unwind(&done);
            return Err(e);
        }
        done.push(link.to.clone());
        out.created += 1;
        out.bytes += link.len;
    }
    Ok(out)
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
        assert_eq!(l.files[0], Entry { rel: "a.mkv".into(), len: 10 });
        assert_eq!(l.files[1], Entry { rel: "b.nfo".into(), len: 20 });
    }

    #[test]
    fn on_disk_puts_multi_file_under_the_root() {
        let t = multi("Pack", &[("a.mkv", 10)]);
        let l = layout(&t).unwrap();
        assert_eq!(l.on_disk("/data", 0).unwrap(), PathBuf::from("/data/Pack/a.mkv"));

        let s = layout(&single("f.mkv", 10, 1)).unwrap();
        assert_eq!(s.on_disk("/data", 0).unwrap(), PathBuf::from("/data/f.mkv"));
    }

    #[test]
    fn a_different_cut_of_the_same_stream_is_refused() {
        // Same total bytes, different boundaries: pairing by index here would
        // link the wrong file to the wrong name.
        let have = layout(&multi("A", &[("one", 100)])).unwrap();
        let want = layout(&multi("B", &[("x", 50), ("y", 50)])).unwrap();
        assert_eq!(plan(&have, "/a", &want, "/b").unwrap_err(), Refusal::LayoutMismatch);
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

        let done = apply(&p).unwrap();
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
                Link { from: src.join("P").join("a"), to: dst.join("P").join("a"), len: 4 },
                Link { from: src.join("P").join("b"), to: dst.join("P").join("b"), len: 4 },
            ],
            cross_device: false,
            bytes: 8,
        };
        assert!(apply(&p).is_err());
        assert!(!dst.join("P").join("a").exists(), "first link was left behind");

        std::fs::remove_dir_all(&dir).unwrap();
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
}
