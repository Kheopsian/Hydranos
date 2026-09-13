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
#[derive(Debug, Clone)]
pub struct Volume {
    /// Mount point. Stable across reboots and meaningful to a human, so this
    /// is what the policy is keyed on and what the UI shows.
    pub id: String,
    pub dev: u64,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub torrents: usize,
    pub policy: Policy,
}

impl Volume {
    pub fn used_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used as f64 * 100.0 / self.total as f64
        }
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

pub fn device_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.dev())
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
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * block;
    let used = (stat.f_blocks as u64 - stat.f_bfree as u64) * block;
    let free = stat.f_bavail as u64 * block;
    Some((used, total, free))
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
    let mut by_dev: HashMap<u64, (PathBuf, usize)> = HashMap::new();
    for t in manager.all() {
        let path = t.save_path.read().clone();
        let Some(dev) = device_of_nearest(&path) else {
            continue;
        };
        let entry = by_dev.entry(dev).or_insert_with(|| (path.clone(), 0));
        entry.1 += 1;
    }
    let mut out: Vec<Volume> = Vec::new();
    for (dev, (sample, count)) in by_dev {
        let mount = mount_point_of(&sample);
        let Some((used, total, free)) = usage(&mount) else {
            continue;
        };
        let id = mount.to_string_lossy().to_string();
        let policy = policy_for(state, &id, cfg);
        out.push(Volume {
            id,
            dev,
            total,
            used,
            free,
            torrents: count,
            policy,
        });
    }
    // Fullest first: the one that needs attention leads the list, and the UI
    // does not have to sort what the API already knows how to order.
    out.sort_by(|a, b| {
        b.used_pct()
            .partial_cmp(&a.used_pct())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}
