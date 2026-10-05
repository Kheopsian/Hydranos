//! Where race data actually lives.
//!
//! Hydranos had no notion of a storage location. One `race_path` was read for
//! every engine of role race, and the drain then acted on `manager.all()`
//! without ever asking where a torrent's bytes were. With two SSDs that is a
//! deletion on the healthy disk to relieve the full one.
//!
//! A volume is not a new setting to type in. It is the `st_dev` of the data
//! that is already on disk, so two categories on one SSD group themselves and
//! two SSDs separate themselves. The name shown to the operator is the mount
//! point, because that is what they recognise -- `st_dev` is a number the
//! kernel is free to change across reboots, which makes it a fine grouping key
//! for one run and a terrible key to store a setting under.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use typhon_engine::torrent::TorrentManager;

/// One filesystem holding race data, with its occupancy and its policy.
///
/// `total`, `used` and `free` are the BASIS every consumer reasons on -- the
/// drain, the admission check and the panel -- and they are only ever filled
/// in by [`Volume::measure`]. Without a quota they are statvfs, as they always
/// were. With one they describe the quota, because on a shared seedbox slot
/// statvfs sees every tenant's data and the percentage it gives is about a
/// disk this node does not own. One constructor, so there is no second formula
/// for some caller to drift onto.
#[derive(Debug, Clone)]
pub struct Volume {
    /// Mount point. Stable across reboots and meaningful to a human, so this
    /// is what the policy is keyed on and what the UI shows.
    pub id: String,
    pub dev: u64,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    /// The capacity the operator declared for this volume, if any. When set it
    /// replaces the disk as the thing the percentages are of.
    pub quota: Option<u64>,
    /// What statvfs says, kept whatever the basis: the physical disk is still
    /// a hard limit under a quota, and only statvfs can tell what a deletion
    /// actually handed back.
    pub disk_total: u64,
    pub disk_used: u64,
    pub disk_free: u64,
    /// Bytes this volume's downloads have PROMISED to write and have not
    /// written yet. The admission check already refuses a new race on
    /// `free - committed`; the drain has to trigger on the same arithmetic or
    /// the two disagree about when the disk is full -- which is exactly the
    /// window where an add is refused and nothing frees anything.
    pub committed: u64,
    pub torrents: usize,
    pub policy: Policy,
}

impl Volume {
    /// The one place occupancy is decided.
    ///
    /// `disk` is statvfs `(used, total, free)`. `data` is what this engine's
    /// torrents on the volume have written, `committed` what they still have to
    /// write -- both from the same walk `alloc_pct` already pays for, so a
    /// quota costs no extra pass over the catalogue.
    ///
    /// Under a quota, used is OUR data rather than the disk's: the neighbours'
    /// files are not ours to count, and counting them is what made the drain
    /// fire on an empty slot or never fire on a full one. Free is the smaller
    /// of what the quota leaves and what the disk has, because a quota larger
    /// than what is physically left does not make room appear.
    #[allow(clippy::too_many_arguments)]
    pub fn measure(
        id: String,
        dev: u64,
        disk: (u64, u64, u64),
        quota: Option<u64>,
        data: u64,
        committed: u64,
        torrents: usize,
        policy: Policy,
    ) -> Self {
        let (disk_used, disk_total, disk_free) = disk;
        // A quota of 0 is "no quota", not "a full volume": a zero-sized basis
        // would read as 0% everywhere and the drain would never fire.
        let quota = quota.filter(|q| *q > 0);
        let (total, used, free) = match quota {
            None => (disk_total, disk_used, disk_free),
            Some(q) => (q, data, q.saturating_sub(data).min(disk_free)),
        };
        Self {
            id,
            dev,
            total,
            used,
            free,
            quota,
            disk_total,
            disk_used,
            disk_free,
            committed,
            torrents,
            policy,
        }
    }

    /// What the percentages are OF, said in the log line that carries them.
    /// A "96%" on a shared slot is useless until one knows whether it is the
    /// tenant's 2 TB or the 40 TB disk under it.
    pub fn basis(&self) -> String {
        match self.quota {
            Some(q) => format!("of {:.1} TB quota", q as f64 / 1e12),
            None => "of disk".to_string(),
        }
    }

    /// Bytes a drain gave back and the occupancy it left, from a fresh statvfs
    /// `(used, total)` taken after the pass.
    ///
    /// Freed is always the DISK delta: declared sizes are what torrents claim,
    /// and only the filesystem knows what it returned (a ghost row frees
    /// nothing). Without a quota the after-percentage is statvfs again, exactly
    /// as before. With one it is our data minus what was freed, on the quota --
    /// re-reading statvfs there would put the neighbours back in the number.
    pub fn after_drain(&self, now: Option<(u64, u64)>) -> (u64, f64) {
        let Some((disk_used_now, disk_total_now)) = now else {
            return (0, self.used_pct());
        };
        let freed = self.disk_used.saturating_sub(disk_used_now);
        let after = match self.quota {
            None => pct(disk_used_now, disk_total_now),
            Some(_) => pct(self.used.saturating_sub(freed), self.total),
        };
        (freed, after)
    }

    /// Fold another race engine's pass over the SAME mount into this one, for
    /// the panel's one-card-per-disk view.
    ///
    /// Without a quota the disk figures are already the whole disk and only the
    /// torrent count adds up -- the shape this view always had. With one, the
    /// basis is our data, so each engine's data and promises add up too, and
    /// free is recomputed through the same rule as `measure`.
    pub fn merge_engine(&mut self, other: &Volume) {
        self.torrents += other.torrents;
        if self.quota.is_none() {
            return;
        }
        let data = self.used.saturating_add(other.used);
        let committed = self.committed.saturating_add(other.committed);
        *self = Volume::measure(
            self.id.clone(),
            self.dev,
            (self.disk_used, self.disk_total, self.disk_free),
            self.quota,
            data,
            committed,
            self.torrents,
            self.policy,
        );
    }

    /// What the disk holds plus what it has already agreed to hold.
    pub fn allocated(&self) -> u64 {
        self.used.saturating_add(self.committed)
    }

    /// Occupancy the drain and the admission check both reason on.
    ///
    /// Can exceed 100: promising more than the disk has is precisely the state
    /// worth reacting to, and clamping it would hide the worst case.
    pub fn alloc_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.allocated() as f64 * 100.0 / self.total as f64
        }
    }

    pub fn used_pct(&self) -> f64 {
        pct(self.used, self.total)
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// What the drain is allowed to do on one volume.
///
/// `inherited` is not a third setting: it records whether these numbers came
/// from the global default or from an override typed for this volume, so the
/// UI can say which disks have a rule of their own without a second request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub enabled: bool,
    pub high: i64,
    pub low: i64,
    pub inherited: bool,
}

impl Policy {
    pub fn global(cfg: &crate::config::RaceDrain) -> Self {
        Self {
            enabled: cfg.enabled,
            high: cfg.high_watermark_pct,
            low: cfg.low_watermark_pct,
            inherited: true,
        }
    }
}

const POLICY_PREFIX: &str = "volume_policy:";

/// Read one volume's override, if it has one.
///
/// In the store rather than in `default.toml` on purpose: a threshold typed
/// into the panel has to apply on the NEXT tick, and a TOML value only applies
/// after a restart. The whole race panel used to carry an "Apply & restart"
/// button for this reason.
pub fn policy_for(state: &crate::api::AppState, mount: &str, cfg: &crate::config::RaceDrain) -> Policy {
    let mut p = Policy::global(cfg);
    let key = format!("{POLICY_PREFIX}{mount}");
    let raw = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.meta_doc(&key)
    };
    let Some(raw) = raw else { return p };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return p;
    };
    if let Some(b) = v.get("enabled").and_then(|x| x.as_bool()) {
        p.enabled = b;
    }
    if let Some(n) = v.get("high").and_then(|x| x.as_i64()) {
        p.high = n;
    }
    if let Some(n) = v.get("low").and_then(|x| x.as_i64()) {
        p.low = n;
    }
    p.inherited = false;
    p
}

pub fn save_policy(state: &crate::api::AppState, mount: &str, p: &Policy) -> anyhow::Result<()> {
    let doc = serde_json::json!({"enabled": p.enabled, "high": p.high, "low": p.low}).to_string();
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    store.put_meta(&format!("{POLICY_PREFIX}{mount}"), &doc)
}

/// Drop a volume's override so it follows the global default again.
pub fn clear_policy(state: &crate::api::AppState, mount: &str) -> anyhow::Result<()> {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    store.put_meta(&format!("{POLICY_PREFIX}{mount}"), "")
}

const QUOTA_PREFIX: &str = "volume_quota:";

/// The capacity declared for a volume, in bytes, if one was.
///
/// Its own key beside the policy rather than a field of it: a quota is a fact
/// about the slot, not a drain rule. Setting one must not freeze the watermarks
/// out of the global default, and "follow the default" on the watermarks must
/// not forget how big the slot is.
pub fn quota_for(state: &crate::api::AppState, mount: &str) -> Option<u64> {
    let raw = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.meta_doc(&format!("{QUOTA_PREFIX}{mount}"))
    }?;
    raw.trim().parse::<u64>().ok().filter(|q| *q > 0)
}

/// Declare, or with `None` drop, a volume's capacity.
pub fn save_quota(state: &crate::api::AppState, mount: &str, bytes: Option<u64>) -> anyhow::Result<()> {
    let doc = bytes.filter(|q| *q > 0).map(|q| q.to_string()).unwrap_or_default();
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    store.put_meta(&format!("{QUOTA_PREFIX}{mount}"), &doc)
}

pub fn device_of(p: &Path) -> Option<u64> {
    crate::platform::volume_id(p)
}

/// Device of the nearest existing ancestor.
///
/// A save path can point at a directory that has not been created yet; that is
/// not a reason to lose the torrent from its volume.
pub fn device_of_nearest(p: &Path) -> Option<u64> {
    let mut cur = p;
    loop {
        if let Some(dev) = device_of(cur) {
            return Some(dev);
        }
        cur = cur.parent()?;
    }
}

/// Bytes used, total and available on the filesystem holding `path`.
///
/// Used is what the filesystem counts as taken, not total minus available: the
/// reserved blocks are neither available to us nor used by us, and counting
/// them as used would drain a disk that is not full.
pub fn usage(path: &Path) -> Option<(u64, u64, u64)> {
    crate::platform::usage(path)
}

/// Mount point of the filesystem a path sits on.
///
/// Walks up until the device changes: the last path that still has the same
/// `st_dev` as its child is the mount point. Reading `/proc/mounts` would name
/// the same place, but it also lists bind mounts and overlays that answer for
/// the same device, and picking among those is guesswork -- walking the tree
/// asks the kernel the question we actually have.
pub fn mount_point_of(path: &Path) -> PathBuf {
    let Some(dev) = device_of_nearest(path) else {
        return path.to_path_buf();
    };
    let mut best = path.to_path_buf();
    let mut cur = path.to_path_buf();
    while let Some(parent) = cur.parent().map(|p| p.to_path_buf()) {
        match device_of(&parent) {
            Some(d) if d == dev => {
                best = parent.clone();
                cur = parent;
            }
            // Parent is on another filesystem, so `cur` is where this one is
            // mounted. Also the exit for a parent we cannot stat at all.
            _ => break,
        }
        if cur.parent().is_none() {
            break;
        }
    }
    best
}

/// The volumes this engine's torrents live on, with occupancy and policy.
///
/// Deduced, never configured. A volume with no torrent on it does not exist
/// for the drain: there is nothing there for it to free.
pub fn discover(
    state: &crate::api::AppState,
    manager: &Arc<TorrentManager>,
    cfg: &crate::config::RaceDrain,
) -> Vec<Volume> {
    let mut by_dev: HashMap<u64, (PathBuf, Tally)> = HashMap::new();
    for t in manager.all() {
        let path = t.save_path.read().clone();
        let Some(dev) = device_of_nearest(&path) else {
            continue;
        };
        let entry = by_dev.entry(dev).or_insert_with(|| (path.clone(), Tally::default()));
        entry.1.add(&t);
    }
    let mut out: Vec<Volume> = Vec::new();
    for (dev, (sample, tally)) in by_dev {
        let mount = mount_point_of(&sample);
        if let Some(v) = build(state, cfg, dev, &mount, tally) {
            out.push(v);
        }
    }
    // Fullest first -- by ALLOCATION, because a disk at 60% with 400 GB in
    // flight needs attention before one at 80% that has finished downloading.
    out.sort_by(|a, b| {
        b.alloc_pct()
            .partial_cmp(&a.alloc_pct())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// The volume a save path lands on, measured the way `discover` measures it.
///
/// For the admission check, which asks about ONE target path. It used to sum
/// its own `committed` and read its own statvfs free; that was a second
/// formula, and a quota added to one of them would have let the drain and the
/// refusal disagree about when the slot is full.
pub fn volume_at(
    state: &crate::api::AppState,
    manager: &Arc<TorrentManager>,
    target: &Path,
    cfg: &crate::config::RaceDrain,
) -> Option<Volume> {
    let dev = device_of_nearest(target)?;
    let mount = mount_point_of(target);
    let mut tally = Tally::default();
    for t in manager.all() {
        let path = t.save_path.read().clone();
        if device_of_nearest(&path) == Some(dev) {
            tally.add(&t);
        }
    }
    build(state, cfg, dev, &mount, tally)
}

/// What one engine's torrents hold on one volume, from a single walk.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    torrents: usize,
    /// Written so far. The quota's "used": Σ total_done.
    data: u64,
    /// Still to be written. Σ (total_size - total_done), never negative.
    committed: u64,
}

impl Tally {
    fn add(&mut self, t: &Arc<typhon_engine::torrent::meta::TorrentState>) {
        self.torrents += 1;
        let core = typhon_engine::rpc::dispatch::torrent_core(t);
        let done = core.total_done.min(t.meta.total_size);
        self.data = self.data.saturating_add(done);
        self.committed = self.committed.saturating_add(t.meta.total_size - done);
    }
}

fn build(
    state: &crate::api::AppState,
    cfg: &crate::config::RaceDrain,
    dev: u64,
    mount: &Path,
    tally: Tally,
) -> Option<Volume> {
    let disk = usage(mount)?;
    let id = mount.to_string_lossy().to_string();
    let policy = policy_for(state, &id, cfg);
    let quota = quota_for(state, &id);
    Some(Volume::measure(id, dev, disk, quota, tally.data, tally.committed, tally.torrents, policy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(enabled: bool, high: i64, low: i64) -> crate::config::RaceDrain {
        let mut d = crate::config::RaceDrain::default();
        d.enabled = enabled;
        d.high_watermark_pct = high;
        d.low_watermark_pct = low;
        d
    }

    const POLICY: Policy = Policy { enabled: true, high: 90, low: 80, inherited: true };

    fn vol(total: u64, used: u64) -> Volume {
        Volume::measure("/mnt".into(), 1, (used, total, total.saturating_sub(used)), None, 0, 0, 0, POLICY)
    }

    /// A slot on a shared disk: `disk_used` of `disk_total` taken by everyone,
    /// `data` of it ours, under a quota of `quota`.
    fn slot(disk_total: u64, disk_used: u64, quota: u64, data: u64, committed: u64) -> Volume {
        Volume::measure(
            "/home/tenant".into(),
            1,
            (disk_used, disk_total, disk_total - disk_used),
            Some(quota),
            data,
            committed,
            3,
            POLICY,
        )
    }

    /// Without a quota nothing may move: the basis is statvfs, field for field,
    /// whatever our own torrents add up to. Every existing install is here.
    #[test]
    fn without_a_quota_the_basis_is_the_disk_exactly() {
        let v = Volume::measure("/mnt".into(), 1, (600, 1000, 350), None, 123, 50, 2, POLICY);
        assert_eq!((v.total, v.used, v.free), (1000, 600, 350));
        assert_eq!((v.disk_total, v.disk_used, v.disk_free), (1000, 600, 350));
        assert_eq!(v.quota, None);
        assert_eq!(v.alloc_pct(), 65.0);
        assert_eq!(v.basis(), "of disk");
    }

    /// The reported bug: a 2 TB slot on a 40 TB disk the neighbours have filled
    /// to 95%. On statvfs the drain fires on our near-empty slot; on the quota
    /// it sees the 25% we actually hold.
    #[test]
    fn under_a_quota_the_neighbours_data_is_not_ours() {
        let v = slot(40_000, 38_000, 2_000, 500, 0);
        assert_eq!(v.total, 2_000, "the percentage is of the quota");
        assert_eq!(v.used, 500, "used is what our torrents wrote, not the disk");
        assert_eq!(v.used_pct(), 25.0);
        assert_eq!(v.alloc_pct(), 25.0);
        assert_eq!(v.free, 1_500);
    }

    /// The other direction: an empty disk, a slot we have filled. On statvfs
    /// this read 5% and the drain never fired.
    #[test]
    fn under_a_quota_a_full_slot_on_an_empty_disk_reads_full() {
        let v = slot(40_000, 2_000, 2_000, 1_900, 0);
        assert_eq!(v.used_pct(), 95.0);
        assert_eq!(v.free, 100);
    }

    /// Same arithmetic as without a quota: written plus promised, on the basis.
    /// Triggering on less would let admission and drain disagree again.
    #[test]
    fn under_a_quota_allocation_still_counts_what_is_to_be_written() {
        let v = slot(40_000, 10_000, 2_000, 1_500, 400);
        assert_eq!(v.used_pct(), 75.0);
        assert_eq!(v.alloc_pct(), 95.0);
        assert_eq!(v.free, 500, "free is the quota minus WRITTEN data; admission subtracts committed");
    }

    /// The disk under the quota is still a hard limit: a quota does not make
    /// room the filesystem does not have.
    #[test]
    fn under_a_quota_free_never_exceeds_what_the_disk_has() {
        let v = slot(40_000, 39_700, 2_000, 500, 0);
        assert_eq!(v.free, 300, "min(quota - used, statvfs free)");
        assert_eq!(v.used_pct(), 25.0, "the percentage stays on the quota");
    }

    /// Past the quota is a state worth reacting to, not a wrap-around.
    #[test]
    fn over_the_quota_free_is_zero_and_the_percentage_says_by_how_much() {
        let v = slot(40_000, 10_000, 2_000, 2_500, 0);
        assert_eq!(v.free, 0);
        assert_eq!(v.used_pct(), 125.0);
    }

    /// A zero quota is the absence of one. Taken literally it would be a
    /// zero-sized basis: 0% everywhere, a drain that never fires.
    #[test]
    fn a_quota_of_zero_is_no_quota() {
        let v = Volume::measure("/mnt".into(), 1, (600, 1000, 400), Some(0), 5, 0, 1, POLICY);
        assert_eq!(v.quota, None);
        assert_eq!((v.total, v.used, v.free), (1000, 600, 400));
    }

    #[test]
    fn the_basis_names_what_the_percentage_is_of() {
        assert_eq!(slot(40_000, 0, 2_000_000_000_000, 0, 0).basis(), "of 2.0 TB quota");
        assert_eq!(vol(100, 1).basis(), "of disk");
    }

    /// Without a quota, the after-drain figure is statvfs re-read, as it was.
    #[test]
    fn after_a_drain_without_a_quota_the_disk_is_read_again() {
        let v = vol(1000, 900);
        assert_eq!(v.after_drain(Some((700, 1000))), (200, 70.0));
        assert_eq!(v.after_drain(None), (0, 90.0), "unreadable: nothing claimed freed");
    }

    /// Under a quota, freed is still the disk delta (only statvfs knows what a
    /// deletion returned) but the occupancy left is ours, on the quota.
    /// Re-reading the disk's used here would report the neighbours' 38 TB.
    #[test]
    fn after_a_drain_under_a_quota_the_occupancy_stays_on_the_quota() {
        let v = slot(40_000, 38_000, 2_000, 1_900, 0);
        let (freed, after) = v.after_drain(Some((37_600, 40_000)));
        assert_eq!(freed, 400);
        assert_eq!(after, 75.0);
    }

    /// Two race engines on one disk are one card. Without a quota only the
    /// count adds up (the disk figures already are the whole disk); under one,
    /// both engines' data counts against the same quota.
    #[test]
    fn merging_engines_adds_data_only_under_a_quota() {
        let mut a = vol(1000, 600);
        a.merge_engine(&vol(1000, 600));
        assert_eq!((a.used, a.total, a.free), (600, 1000, 400), "the disk is not counted twice");

        let mut q = slot(40_000, 10_000, 2_000, 800, 100);
        q.merge_engine(&slot(40_000, 10_000, 2_000, 700, 50));
        assert_eq!(q.used, 1_500);
        assert_eq!(q.committed, 150);
        assert_eq!(q.free, 500);
        assert_eq!(q.torrents, 6);
    }

    /// The quota lives in the store under its own key and survives the policy
    /// being dropped back to the default.
    #[test]
    fn a_quota_is_stored_apart_from_the_policy() {
        let s = crate::api::testing::state_from(
            "volume-quota",
            "[daemon]\napi_key = \"0123456789abcdef0123456789abcdef\"\n",
        );
        assert_eq!(quota_for(&s.state, "/home/tenant"), None);
        save_quota(&s.state, "/home/tenant", Some(2_000_000_000_000)).unwrap();
        assert_eq!(quota_for(&s.state, "/home/tenant"), Some(2_000_000_000_000));
        assert_eq!(quota_for(&s.state, "/home/other"), None, "keyed per mount");
        clear_policy(&s.state, "/home/tenant").unwrap();
        assert_eq!(quota_for(&s.state, "/home/tenant"), Some(2_000_000_000_000));
        save_quota(&s.state, "/home/tenant", None).unwrap();
        assert_eq!(quota_for(&s.state, "/home/tenant"), None);
    }

    /// A volume of size zero is not a full volume. Dividing by its total would
    /// hand the drain a NaN, and `sort_by` on NaN silently keeps the input
    /// order -- the fullest disk would stop leading the list.
    #[test]
    fn a_volume_with_no_size_is_zero_percent_not_nan() {
        let v = vol(0, 0);
        assert_eq!(v.used_pct(), 0.0);
        assert!(!v.used_pct().is_nan());
    }

    #[test]
    /// The whole point of the change: a disk that LOOKS half empty can already
    /// be full, and that is the state an add gets refused in.
    #[test]
    fn allocation_counts_what_is_still_to_be_written() {
        let mut v = vol(1000, 600);
        v.committed = 350;
        assert_eq!(v.used_pct(), 60.0, "occupancy is what statvfs says");
        assert_eq!(v.alloc_pct(), 95.0, "allocation is occupancy plus what is promised");
    }

    /// Promising more than the disk holds is exactly the case worth reacting
    /// to. Clamping it to 100 would erase the severity.
    #[test]
    fn allocation_may_exceed_one_hundred_percent() {
        let mut v = vol(1000, 900);
        v.committed = 400;
        assert_eq!(v.alloc_pct(), 130.0);
    }

    /// With nothing in flight the new rule must behave exactly like the old
    /// one, otherwise every idle disk changes behaviour on upgrade.
    #[test]
    fn with_nothing_in_flight_allocation_is_occupancy() {
        let v = vol(1000, 830);
        assert_eq!(v.alloc_pct(), v.used_pct());
    }

    #[test]
    fn used_pct_is_used_over_total() {
        assert_eq!(vol(100, 50).used_pct(), 50.0);
        assert_eq!(vol(1000, 1).used_pct(), 0.1);
        assert_eq!(vol(100, 100).used_pct(), 100.0);
    }

    /// `inherited` is the whole point of `global`: it is what lets the panel
    /// say "this disk has no rule of its own" without a second request.
    #[test]
    fn the_global_policy_is_marked_inherited() {
        let p = Policy::global(&drain(true, 91, 77));
        assert!(p.inherited, "a policy built from the global default is inherited");
        assert!(p.enabled);
        assert_eq!(p.high, 91);
        assert_eq!(p.low, 77);

        let off = Policy::global(&drain(false, 10, 5));
        assert!(!off.enabled);
        assert!(off.inherited);
    }

    /// The OS temp directory, which exists on every platform this builds for.
    /// These tests used to write "/tmp", which on Windows is neither absolute
    /// nor present -- and the assertions still passed, because the Win32 path
    /// lookup answered for a directory that was not there.
    fn tmp() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn device_of_answers_for_a_path_that_exists_and_not_for_one_that_does_not() {
        assert!(device_of(&tmp()).is_some());
        assert!(device_of(&tmp().join("typhon-no-such-path-6f1a2b")).is_none());
    }

    /// A save path may point at a directory nobody has created yet. That is
    /// not a reason to lose the torrent from its volume.
    #[test]
    fn a_save_path_not_yet_created_still_resolves_to_its_volume() {
        let deep = tmp()
            .join("typhon-absent-a")
            .join("typhon-absent-b")
            .join("typhon-absent-c");
        let got = device_of_nearest(&deep).expect("walks up to the temp dir, which exists");
        assert_eq!(got, device_of(&tmp()).unwrap());
    }

    /// Walking up from an existing path must land on a real ancestor that is
    /// still on the same filesystem -- that is the definition of the mount
    /// point the UI shows.
    #[test]
    fn the_mount_point_is_an_ancestor_on_the_same_device() {
        let p = tmp();
        let mount = mount_point_of(&p);
        assert!(
            p.starts_with(&mount) || mount.starts_with(&p),
            "{mount:?} must be on the same branch as the temp dir"
        );
        assert_eq!(
            device_of(&mount).unwrap(),
            device_of_nearest(&p).unwrap(),
            "the mount point sits on the same filesystem as the path"
        );
    }

    /// Unstattable path: the function must still name something rather than
    /// panic, because a save path can be anywhere.
    #[test]
    fn an_unreachable_path_is_its_own_mount_point() {
        // Nothing under a path that cannot exist is stattable, and `/` always
        // is, so the walk terminates either way.
        let mount = mount_point_of(&tmp().join("typhon-absent-x").join("y"));
        assert!(mount.is_absolute());
    }

    /// Used is what the filesystem counts as taken, NOT total minus available:
    /// reserved blocks belong to neither, and counting them as used drains a
    /// disk that is not full.
    #[test]
    fn usage_does_not_count_reserved_blocks_as_used() {
        let (used, total, free) = usage(&tmp()).expect("the temp dir is on a filesystem");
        assert!(total > 0, "a mounted filesystem has a size");
        assert!(used <= total);
        assert!(free <= total);
        assert!(
            used + free <= total,
            "used {used} + free {free} must leave room for reserved blocks in {total}"
        );
    }

    #[test]
    fn usage_of_a_path_that_does_not_exist_is_none() {
        assert!(usage(&tmp().join("typhon-no-such-path-6f1a2b")).is_none());
    }

    /// An interior NUL cannot be handed to statvfs; that is a None, not a panic.
    ///
    /// Unix only: the Windows path goes through encode_wide, which has no
    /// CString conversion to fail, so there is nothing to assert there.
    #[cfg(unix)]
    #[test]
    fn usage_rejects_a_path_with_an_interior_nul() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = PathBuf::from(OsStr::from_bytes(b"/tmp/a\0b"));
        assert!(usage(&bad).is_none());
    }
}
