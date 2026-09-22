//! Who else holds these bytes.
//!
//! A torrent whose files are hardlinked somewhere is not necessarily a torrent
//! worth keeping. `nlink` counts the names an inode has; it does not say whose
//! they are. Two cross-seeded torrents linking to each other both report
//! `nlink = 2` while nothing outside Hydranos refers to their bytes at all --
//! delete both and nothing of value is lost, yet a rule keyed on `link_count`
//! would protect them forever.
//!
//! So the number that matters is the one nobody stores:
//!
//! ```text
//! external_links = nlink - (names this catalogue holds)
//! ```
//!
//! Zero means every name belongs to us. The media library, a backup, a folder
//! someone made by hand -- any of them push it above zero, without us having to
//! know where they are. That is the property a list of protected paths cannot
//! have: it only protects the places somebody remembered to configure.
//!
//! ⚠️ `owned` counts NAMES, not holders. Two cross-seeded torrents routinely
//! point at the same path -- one name, not two -- and counting holders would
//! charge that inode twice, drive `external_links` to zero and mark a file the
//! library is using as free to delete. Dedup by path before counting, always.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::platform::FileId;

/// What one torrent looks like once the catalogue has been counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinkFacts {
    /// Names held by someone outside this catalogue, over the torrent's files.
    ///
    /// The MAXIMUM across files, not the minimum or the sum: one file still in
    /// use is enough to make removing the torrent destructive, and the fact is
    /// read as "is anyone else using this", so it has to fail safe.
    pub external_links: u64,
    /// Bytes that would actually come back. Only files at `nlink == 1` count:
    /// unlinking a name an inode shares with another frees nothing.
    pub freeable_bytes: u64,
    /// Highest `nlink` across the files. Kept for display; it is the number
    /// that misleads, so nothing should key a deletion on it.
    pub link_count: u64,
    /// Not one file could be stat'd. A seeding torrent in this state is
    /// announcing data it cannot serve.
    pub data_missing: bool,
}

/// One torrent as this module needs it: its hash, and its files already
/// resolved to absolute paths with their stat, `None` where the stat failed.
pub type Entry = (String, Vec<(PathBuf, Option<FileId>)>);

/// Count the catalogue, then answer for each torrent. Pure: every syscall has
/// already happened by the time this is called, which is what lets the whole
/// table be exercised from a unit test with no filesystem at all.
///
/// Both passes are over the same slice, and both are needed: a torrent's
/// answer depends on names held by OTHER torrents, so nothing can be decided
/// until every file in the catalogue has been seen.
pub fn compute(entries: &[Entry]) -> HashMap<String, LinkFacts> {
    let mut owned: HashMap<(u64, u64), u64> = HashMap::new();
    let mut seen: HashSet<&Path> = HashSet::new();

    for (_, files) in entries {
        for (path, st) in files {
            let Some(st) = st else { continue };
            // The dedup that keeps two cross-seeds of one path from counting
            // as two names. See the module note.
            if seen.insert(path.as_path()) {
                *owned.entry((st.volume, st.index)).or_insert(0) += 1;
            }
        }
    }

    let mut out = HashMap::with_capacity(entries.len());
    for (hash, files) in entries {
        let mut f = LinkFacts {
            data_missing: true,
            ..Default::default()
        };
        for (_, st) in files {
            let Some(st) = st else { continue };
            f.data_missing = false;
            f.link_count = f.link_count.max(st.links);
            if st.links == 1 {
                f.freeable_bytes += st.size;
            }
            let held = owned.get(&(st.volume, st.index)).copied().unwrap_or(0);
            // ⚠️ `held > nlink` cannot happen unless the dedup above is wrong.
            // If it ever does, the honest answer is "someone else may hold
            // this", never "free to delete": saturating to zero here would turn
            // a counting bug into deleted files.
            let ext = if held > st.links {
                1
            } else {
                st.links - held
            };
            f.external_links = f.external_links.max(ext);
        }
        out.insert(hash.clone(), f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(index: u64, links: u64, size: u64) -> Option<FileId> {
        Some(FileId {
            volume: 1,
            index,
            links,
            size,
        })
    }
    fn e(hash: &str, files: &[(&str, Option<FileId>)]) -> Entry {
        (
            hash.to_string(),
            files
                .iter()
                .map(|(p, s)| (PathBuf::from(p), *s))
                .collect(),
        )
    }

    /// The five cases the bench fixture builds on disk, as one table.
    #[test]
    fn the_fixture_cases() {
        let got = compute(&[
            // A: one name ours, one in the library.
            e("A", &[("/t/A.bin", st(1, 2, 1000))]),
            // B: ours, the library's, and a cross-seed of ours.
            e("B", &[("/t/B.bin", st(2, 3, 1000))]),
            e("Bx", &[("/x/B.bin", st(2, 3, 1000))]),
            // C: ours and a cross-seed of ours. Nobody else. THE case.
            e("C", &[("/t/C.bin", st(3, 2, 1000))]),
            e("Cx", &[("/x/C.bin", st(3, 2, 1000))]),
            // D: no links at all.
            e("D", &[("/t/D.bin", st(4, 1, 1000))]),
            // E: two torrents, ONE path. One name, not two.
            e("E1", &[("/t/E.bin", st(5, 2, 1000))]),
            e("E2", &[("/t/E.bin", st(5, 2, 1000))]),
        ]);

        assert_eq!(got["A"].external_links, 1, "the library holds a name");
        assert_eq!(got["B"].external_links, 1, "the library still holds one");
        assert_eq!(got["C"].external_links, 0, "every name is ours");
        assert_eq!(got["D"].external_links, 0, "there is only our name");
        assert_eq!(
            got["E1"].external_links, 1,
            "two torrents sharing ONE path is one owned name, not two"
        );
        assert_eq!(got["E2"].external_links, 1);
    }

    /// A and C both report `nlink = 2` and mean opposite things. This is the
    /// whole reason the module exists.
    #[test]
    fn link_count_cannot_tell_a_from_c() {
        let got = compute(&[
            e("A", &[("/t/A.bin", st(1, 2, 1000))]),
            e("C", &[("/t/C.bin", st(3, 2, 1000))]),
            e("Cx", &[("/x/C.bin", st(3, 2, 1000))]),
        ]);
        assert_eq!(got["A"].link_count, got["C"].link_count);
        assert_ne!(got["A"].external_links, got["C"].external_links);
    }

    #[test]
    fn only_unshared_files_are_counted_as_freeable() {
        let got = compute(&[
            e("shared", &[("/t/s.bin", st(1, 2, 4096))]),
            e("alone", &[("/t/a.bin", st(2, 1, 4096))]),
        ]);
        assert_eq!(
            got["shared"].freeable_bytes, 0,
            "unlinking one of two names frees nothing"
        );
        assert_eq!(got["alone"].freeable_bytes, 4096);
    }

    /// One file still in use protects the whole torrent.
    #[test]
    fn a_multi_file_torrent_takes_the_highest_external_count() {
        let got = compute(&[e(
            "m",
            &[
                ("/t/m/1.mkv", st(1, 1, 10)),
                ("/t/m/2.mkv", st(2, 2, 10)),
                ("/t/m/3.mkv", st(3, 1, 10)),
            ],
        )]);
        assert_eq!(got["m"].external_links, 1);
        assert_eq!(got["m"].freeable_bytes, 20, "only the two unshared ones");
    }

    #[test]
    fn a_torrent_with_no_readable_file_is_flagged_not_guessed() {
        let got = compute(&[e("gone", &[("/t/gone.bin", None)])]);
        assert!(got["gone"].data_missing);
        assert_eq!(
            got["gone"].external_links, 0,
            "nothing was measured, so nothing is claimed"
        );
    }

    /// Not reachable through `compute`'s own dedup, but the guard has to hold
    /// on its own: an over-count must never read as "free to delete".
    #[test]
    fn an_impossible_owner_count_fails_safe() {
        let mut entries = vec![e("a", &[("/t/x.bin", st(9, 1, 10))])];
        entries.push(e("b", &[("/t/y.bin", st(9, 1, 10))]));
        let got = compute(&entries);
        assert_eq!(
            got["a"].external_links, 1,
            "two names on a one-link inode is incoherent, so keep"
        );
    }
}
