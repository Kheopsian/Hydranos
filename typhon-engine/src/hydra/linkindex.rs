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

/// Does a fresh measurement still justify the deletion the pass decided on?
///
/// The decision, alone, so it can be exercised without an engine or a store.
/// `link_guard` is the plumbing that fetches the two arguments.
///
/// ⭐ Only an INCREASE refuses. A name that appeared since the scan means
/// somebody took an interest in these bytes, and that is the whole premise of
/// `external_links == 0` gone. A name that DISAPPEARED leaves the cached count
/// too high, which only ever protects a torrent that could have been removed:
/// the harmless direction, and one more pass will catch it.
///
/// ⚠️ Not a re-evaluation of the rule. The condition may have stopped holding
/// for a dozen reasons between the pass and the action; this guards the single
/// one whose cost cannot be undone.
pub fn guard_verdict(fresh: &LinkFacts, cached: &LinkFacts) -> Result<(), String> {
    if fresh.data_missing && !cached.data_missing {
        return Err("refused: the files are no longer readable".into());
    }
    if fresh.external_links > cached.external_links {
        return Err(format!(
            "refused: {} external link(s) now, {} when the catalogue was scanned",
            fresh.external_links, cached.external_links
        ));
    }
    Ok(())
}

/// Bytes one file takes in a stored measurement: a presence flag, then
/// volume, index, links and size as little-endian u64.
const PACKED_FILE: usize = 33;

/// One torrent's measurement, in file order, as the store keeps it.
///
/// A BLOB per torrent rather than a row per file: a million torrents are some
/// three million files, and a row each would mean three million keys to write
/// and to read back for every pass. The paths are NOT stored -- they are
/// derived from the torrent's own metadata and its save path, the same way the
/// measurement was taken, so a stored row is only valid against the save path
/// it was measured under (the store keeps that beside it).
pub fn pack(stats: &[Option<FileId>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(stats.len() * PACKED_FILE);
    for st in stats {
        match st {
            Some(id) => {
                out.push(1);
                for v in [id.volume, id.index, id.links, id.size] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            None => out.extend_from_slice(&[0u8; PACKED_FILE]),
        }
    }
    out
}

/// The reverse of `pack`. `None` for a blob that is not a whole number of
/// files: a truncated measurement is treated as no measurement, never as a
/// shorter torrent whose missing tail would read as "no names of ours".
pub fn unpack(blob: &[u8]) -> Option<Vec<Option<FileId>>> {
    if blob.len() % PACKED_FILE != 0 {
        return None;
    }
    let word = |c: &[u8], i: usize| u64::from_le_bytes(c[1 + i * 8..9 + i * 8].try_into().unwrap());
    Some(
        blob.chunks_exact(PACKED_FILE)
            .map(|c| {
                (c[0] == 1).then(|| FileId {
                    volume: word(c, 0),
                    index: word(c, 1),
                    links: word(c, 2),
                    size: word(c, 3),
                })
            })
            .collect(),
    )
}

/// Files of a measurement that could not be read.
pub fn missing_files(stats: &[Option<FileId>]) -> usize {
    stats.iter().filter(|s| s.is_none()).count()
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

    fn lf(external: u64, links: u64) -> LinkFacts {
        LinkFacts {
            external_links: external,
            link_count: links,
            freeable_bytes: 0,
            data_missing: false,
        }
    }

    #[test]
    fn the_guard_refuses_a_name_that_appeared_since_the_scan() {
        let e = guard_verdict(&lf(1, 3), &lf(0, 2)).unwrap_err();
        assert!(e.contains("1 external link(s) now, 0"), "{e}");
    }

    #[test]
    fn the_guard_lets_through_what_it_measured() {
        assert!(guard_verdict(&lf(0, 2), &lf(0, 2)).is_ok());
    }

    /// A rule may legitimately delete at a non-zero count. What matters is that
    /// the number did not GROW, not that it is zero.
    #[test]
    fn the_guard_is_about_growth_not_about_zero() {
        assert!(guard_verdict(&lf(2, 5), &lf(2, 5)).is_ok());
        assert!(guard_verdict(&lf(3, 6), &lf(2, 5)).is_err());
    }

    #[test]
    fn a_name_removed_since_the_scan_does_not_refuse() {
        assert!(
            guard_verdict(&lf(0, 1), &lf(1, 2)).is_ok(),
            "fewer holders than measured only ever protects too much"
        );
    }

    #[test]
    fn the_guard_refuses_when_the_files_went_missing() {
        let gone = LinkFacts {
            data_missing: true,
            ..Default::default()
        };
        assert!(
            guard_verdict(&gone, &lf(0, 1)).is_err(),
            "unreadable is not the same as unwanted"
        );
    }

    /// And a torrent already known to be missing is not blocked by that alone.
    #[test]
    fn already_missing_at_scan_time_is_not_a_refusal() {
        let gone = LinkFacts {
            data_missing: true,
            ..Default::default()
        };
        assert!(guard_verdict(&gone, &gone).is_ok());
    }

    #[test]
    fn a_measurement_comes_back_from_the_store_as_it_went_in() {
        let stats = vec![st(7, 3, 4096), None, st(u64::MAX, 1, 0)];
        let blob = pack(&stats);
        assert_eq!(blob.len(), 3 * PACKED_FILE);
        assert_eq!(unpack(&blob), Some(stats.clone()));
        assert_eq!(missing_files(&stats), 1);
        assert_eq!(unpack(&[]), Some(Vec::new()), "a torrent with no file is still a measurement");
    }

    #[test]
    fn a_truncated_measurement_is_no_measurement() {
        let mut blob = pack(&[st(1, 1, 1), st(2, 1, 1)]);
        blob.pop();
        assert_eq!(unpack(&blob), None);
    }

    /// The guard that stands between a stored measurement and `delete`.
    #[test]
    fn a_recheck_sees_a_name_added_since_the_scan() {
        let dir = std::env::temp_dir().join(format!("hydranos-recheck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let a = dir.join("a.bin");
        std::fs::write(&a, b"0123456789").expect("write");

        // What the scan concluded: one name, ours, nobody else. Deletable.
        let cached = LinkFacts {
            external_links: 0,
            link_count: 1,
            freeable_bytes: 10,
            data_missing: false,
        };
        let files = vec![a.clone()];
        assert_eq!(recheck(&files, &cached).external_links, 0);

        // Then the media library hardlinks it, as it would between two passes.
        let b = dir.join("b.mkv");
        std::fs::hard_link(&a, &b).expect("hard_link");
        let now = recheck(&files, &cached);
        assert_eq!(
            now.external_links, 1,
            "a name appeared since the scan, so the torrent is no longer free to delete"
        );
        assert_eq!(now.freeable_bytes, 0, "and unlinking one of two frees nothing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recheck_of_a_vanished_file_reports_missing_not_deletable() {
        let missing = std::env::temp_dir().join("hydranos-recheck-absent-7c1f");
        let r = recheck(&[missing], &LinkFacts::default());
        assert!(r.data_missing);
        assert_eq!(r.external_links, 0, "nothing measured, nothing claimed");
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

/// Re-measure ONE torrent, against the catalogue the scan already counted.
///
/// ⭐ The guard for irreversible actions. `owned` cannot be recomputed for one
/// torrent alone -- it is a property of the whole catalogue -- so the cached
/// count is reused while the `nlink` values are read fresh. A name added since
/// the scan raises `nlink`, which raises `external_links`, which is the
/// direction that stops a deletion. A name REMOVED since the scan lowers it,
/// and there the stale count is the conservative one anyway.
pub fn recheck(files: &[PathBuf], cached: &LinkFacts) -> LinkFacts {
    let mut out = LinkFacts {
        data_missing: true,
        ..Default::default()
    };
    let mut any_unknown = false;
    for p in files {
        let Some(id) = crate::platform::file_id(p) else {
            continue;
        };
        out.data_missing = false;
        out.link_count = out.link_count.max(id.links);
        if id.links == 1 {
            out.freeable_bytes += id.size;
        }
        // How many of this inode's names the catalogue held at scan time. The
        // cached torrent-level count is the best available: if it looks
        // impossible against the live nlink, say "someone else holds this".
        let held = cached.link_count.saturating_sub(cached.external_links);
        if held > id.links || held == 0 {
            any_unknown = true;
        }
        let ext = if held > id.links { 1 } else { id.links - held };
        out.external_links = out.external_links.max(ext);
    }
    if any_unknown && out.external_links == 0 {
        out.external_links = 1;
    }
    out
}
