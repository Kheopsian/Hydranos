//! Share limits, as qBittorrent has them: a ratio, a seeding time and an
//! inactive seeding time, past ANY of which a seed is stopped or removed.
//!
//! Two levels. The engine's (`max_ratio`, `max_seeding_time`,
//! `max_inactive_seeding_time`, `share_limit_action` in `[race]` / `[hoard]`
//! or an `[[engine]]` session), all off by default; and the torrent's own, in
//! the store's sparse `share_limits` table, where -2 follows the engine and
//! -1 is "no limit" -- `setShareLimits`' own encoding, so the shim stores what
//! it is sent.
//!
//! Distinct from the workflows on purpose: a workflow is a rule the operator
//! writes, this is the client feature *arr and autobrr expect of a
//! qBittorrent, and a client that reads `max_ratio_enabled` must find it here.
//!
//! The worker that acts on them is `workers::spawn_share_limits`.

use std::collections::HashMap;

use crate::api::AppState;

/// `-2`: the torrent follows its engine.
pub const FOLLOW_ENGINE: i64 = -2;
/// `-1`: no limit.
pub const NO_LIMIT: i64 = -1;

/// One torrent's own limits. -2 = the engine's, -1 = none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TorrentShareLimits {
    pub ratio: f64,
    pub seeding_minutes: i64,
    pub inactive_minutes: i64,
}

impl Default for TorrentShareLimits {
    fn default() -> Self {
        TorrentShareLimits {
            ratio: FOLLOW_ENGINE as f64,
            seeding_minutes: FOLLOW_ENGINE,
            inactive_minutes: FOLLOW_ENGINE,
        }
    }
}

/// What happens to a seed that reached a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    #[default]
    Stop,
    Remove,
    RemoveWithFiles,
}

impl Action {
    pub fn parse(raw: &str) -> Option<Action> {
        match raw.trim() {
            "" | "stop" | "pause" => Some(Action::Stop),
            "remove" => Some(Action::Remove),
            "remove_with_files" => Some(Action::RemoveWithFiles),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Action::Stop => "stop",
            Action::Remove => "remove",
            Action::RemoveWithFiles => "remove_with_files",
        }
    }

    /// qBittorrent's `max_ratio_act` (its `ShareLimitAction`): 0 stop,
    /// 1 remove, 2 super seeding, 3 remove with the files.
    pub fn qbit_code(self) -> i64 {
        match self {
            Action::Stop => 0,
            Action::Remove => 1,
            Action::RemoveWithFiles => 3,
        }
    }

    /// `None` for 2 (super seeding, which this engine does not do) and for
    /// anything qBittorrent does not define: the action already set stays.
    pub fn from_qbit(code: i64) -> Option<Action> {
        match code {
            0 => Some(Action::Stop),
            1 => Some(Action::Remove),
            3 => Some(Action::RemoveWithFiles),
            _ => None,
        }
    }
}

/// One engine's limits, -1 = off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineShareLimits {
    pub ratio: f64,
    pub seeding_minutes: i64,
    pub inactive_minutes: i64,
    pub action: Action,
}

impl Default for EngineShareLimits {
    fn default() -> Self {
        EngineShareLimits {
            ratio: NO_LIMIT as f64,
            seeding_minutes: NO_LIMIT,
            inactive_minutes: NO_LIMIT,
            action: Action::Stop,
        }
    }
}

impl EngineShareLimits {
    /// Absent and negative are both off. An action nobody can read is a
    /// stop, the one that can be undone, and it is said once at load.
    pub fn from_session(s: &crate::config::Session) -> EngineShareLimits {
        let off_f = |v: Option<f64>| match v {
            Some(x) if x.is_finite() && x >= 0.0 => x,
            _ => NO_LIMIT as f64,
        };
        let off_i = |v: Option<i64>| match v {
            Some(x) if x >= 0 => x,
            _ => NO_LIMIT,
        };
        EngineShareLimits {
            ratio: off_f(s.max_ratio),
            seeding_minutes: off_i(s.max_seeding_time),
            inactive_minutes: off_i(s.max_inactive_seeding_time),
            action: s
                .share_limit_action
                .as_deref()
                .and_then(Action::parse)
                .unwrap_or_default(),
        }
    }

    pub fn any(&self) -> bool {
        self.ratio >= 0.0 || self.seeding_minutes >= 0 || self.inactive_minutes >= 0
    }
}

/// The limits that apply to one torrent: its own where it has one, its
/// engine's elsewhere. -1 = none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effective {
    pub ratio: f64,
    pub seeding_minutes: i64,
    pub inactive_minutes: i64,
}

impl Effective {
    pub fn any(&self) -> bool {
        self.ratio >= 0.0 || self.seeding_minutes >= 0 || self.inactive_minutes >= 0
    }
}

pub fn effective(own: &TorrentShareLimits, engine: &EngineShareLimits) -> Effective {
    Effective {
        ratio: if own.ratio <= FOLLOW_ENGINE as f64 { engine.ratio } else { own.ratio },
        seeding_minutes: if own.seeding_minutes == FOLLOW_ENGINE {
            engine.seeding_minutes
        } else {
            own.seeding_minutes
        },
        inactive_minutes: if own.inactive_minutes == FOLLOW_ENGINE {
            engine.inactive_minutes
        } else {
            own.inactive_minutes
        },
    }
}

/// A torrent ratio limit as `setShareLimits` sends it, onto the three values
/// it can mean. Anything under -1 is "follow" (qBit sends -2 exactly, but a
/// float is never compared for equality here).
pub fn normalize_ratio(v: f64) -> f64 {
    if !v.is_finite() || v < -1.5 {
        FOLLOW_ENGINE as f64
    } else if v < 0.0 {
        NO_LIMIT as f64
    } else {
        v
    }
}

pub fn normalize_minutes(v: i64) -> i64 {
    if v <= FOLLOW_ENGINE {
        FOLLOW_ENGINE
    } else if v < 0 {
        NO_LIMIT
    } else {
        v
    }
}

/// Which limit a seed reached, with the figure and the limit, for the log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reached {
    Ratio { ratio: f64, limit: f64 },
    SeedingTime { minutes: i64, limit: i64 },
    Inactive { minutes: i64, limit: i64 },
}

impl std::fmt::Display for Reached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reached::Ratio { ratio, limit } => write!(f, "ratio {ratio:.2} >= {limit}"),
            Reached::SeedingTime { minutes, limit } => write!(f, "seeded {minutes} min >= {limit} min"),
            Reached::Inactive { minutes, limit } => {
                write!(f, "nothing uploaded for {minutes} min >= {limit} min")
            }
        }
    }
}

/// qBittorrent's test: ratio OR seeding time OR inactivity, each `>=`.
/// `inactive_secs` is `None` when inactivity is not tracked for this one.
pub fn reached(
    eff: &Effective,
    ratio: impl FnOnce() -> f64,
    seeding_secs: i64,
    inactive_secs: Option<i64>,
) -> Option<Reached> {
    if eff.ratio >= 0.0 {
        let r = ratio();
        if r >= eff.ratio {
            return Some(Reached::Ratio { ratio: r, limit: eff.ratio });
        }
    }
    if eff.seeding_minutes >= 0 && seeding_secs >= eff.seeding_minutes * 60 {
        return Some(Reached::SeedingTime { minutes: seeding_secs / 60, limit: eff.seeding_minutes });
    }
    if eff.inactive_minutes >= 0 {
        if let Some(idle) = inactive_secs {
            if idle >= eff.inactive_minutes * 60 {
                return Some(Reached::Inactive { minutes: idle / 60, limit: eff.inactive_minutes });
            }
        }
    }
    None
}

// --- reading them off the live state ----------------------------------------

/// One engine's limits, from the live config. An engine the config does not
/// name (a remote one) has none.
pub fn engine_limits(state: &AppState, engine_id: &str) -> EngineShareLimits {
    engine_limits_in(&state.cfg(), engine_id)
}

pub fn engine_limits_in(cfg: &crate::config::Config, engine_id: &str) -> EngineShareLimits {
    cfg.local_engines()
        .into_iter()
        .find(|l| l.id == engine_id)
        .map(|l| EngineShareLimits::from_session(&l.session))
        .unwrap_or_default()
}

/// Every torrent's own limits, or none when the store cannot say.
pub fn overrides(state: &AppState) -> HashMap<String, TorrentShareLimits> {
    state
        .store
        .read()
        .ok()
        .and_then(|s| s.share_limits_all().ok())
        .unwrap_or_default()
}

/// The six fields qBittorrent's `torrents/info` and `properties` carry: the
/// torrent's own (`ratio_limit`, `seeding_time_limit`,
/// `inactive_seeding_time_limit`, -2 = follow) and the ones in force
/// (`max_*`, -1 = none).
pub fn qbit_fields(own: &TorrentShareLimits, engine: &EngineShareLimits) -> serde_json::Value {
    let eff = effective(own, engine);
    serde_json::json!({
        "ratio_limit": crate::row::num_json(own.ratio),
        "seeding_time_limit": own.seeding_minutes,
        "inactive_seeding_time_limit": own.inactive_minutes,
        "max_ratio": crate::row::num_json(eff.ratio),
        "max_seeding_time": eff.seeding_minutes,
        "max_inactive_seeding_time": eff.inactive_minutes,
    })
}

/// Lay `qbit_fields` over a shim row.
pub fn fill_qbit_row(row: &mut serde_json::Value, own: &TorrentShareLimits, engine: &EngineShareLimits) {
    if let (Some(obj), serde_json::Value::Object(fields)) = (row.as_object_mut(), qbit_fields(own, engine)) {
        obj.extend(fields);
    }
}

/// The native view of one torrent's limits on one engine.
pub fn native_json(engine_id: &str, own: &TorrentShareLimits, engine: &EngineShareLimits) -> serde_json::Value {
    let eff = effective(own, engine);
    serde_json::json!({
        "engine": engine_id,
        "ratio_limit": crate::row::num_json(own.ratio),
        "seeding_time_limit": own.seeding_minutes,
        "inactive_seeding_time_limit": own.inactive_minutes,
        "effective": {
            "ratio": crate::row::num_json(eff.ratio),
            "seeding_time": eff.seeding_minutes,
            "inactive_seeding_time": eff.inactive_minutes,
        },
        "action": engine.action.as_str(),
    })
}

/// The engine level as the native routes show it.
pub fn engine_json(engine_id: &str, e: &EngineShareLimits) -> serde_json::Value {
    serde_json::json!({
        "engine": engine_id,
        "max_ratio": crate::row::num_json(e.ratio),
        "max_seeding_time": e.seeding_minutes,
        "max_inactive_seeding_time": e.inactive_minutes,
        "share_limit_action": e.action.as_str(),
    })
}

/// Set the torrents' own limits, in one store transaction. `None` keeps a
/// field. Values are normalized onto -2 / -1 / a limit first.
pub fn set_torrent_limits(
    state: &AppState,
    hashes: &[String],
    ratio: Option<f64>,
    seeding_minutes: Option<i64>,
    inactive_minutes: Option<i64>,
) -> Result<usize, String> {
    let store = state.store.lock().map_err(|_| "the store is unavailable".to_string())?;
    store
        .set_share_limits_batch(
            hashes,
            ratio.map(normalize_ratio),
            seeding_minutes.map(normalize_minutes),
            inactive_minutes.map(normalize_minutes),
        )
        .map_err(|e| e.to_string())
}

/// An engine-level change: `Some` sets the key (-1 to switch it off), `None`
/// leaves it.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineChange {
    #[serde(default)]
    pub max_ratio: Option<f64>,
    #[serde(default)]
    pub max_seeding_time: Option<i64>,
    #[serde(default)]
    pub max_inactive_seeding_time: Option<i64>,
    #[serde(default)]
    pub share_limit_action: Option<String>,
}

impl EngineChange {
    pub fn is_empty(&self) -> bool {
        self.max_ratio.is_none()
            && self.max_seeding_time.is_none()
            && self.max_inactive_seeding_time.is_none()
            && self.share_limit_action.is_none()
    }

    /// Whether applying this to `current` would change nothing. A client
    /// that posts back the whole preferences page it just read (qBittorrent's
    /// own WebUI does) must not rewrite every engine with the one engine's
    /// values that page shows.
    pub fn is_noop_for(&self, current: &EngineShareLimits) -> bool {
        let ratio_same = self.max_ratio.is_none_or(|r| {
            let r = if r.is_finite() && r >= 0.0 { r } else { -1.0 };
            (r - current.ratio).abs() < 1e-9
        });
        ratio_same
            && self.max_seeding_time.is_none_or(|m| m.max(-1) == current.seeding_minutes)
            && self.max_inactive_seeding_time.is_none_or(|m| m.max(-1) == current.inactive_minutes)
            && self
                .share_limit_action
                .as_deref()
                .is_none_or(|a| Action::parse(a) == Some(current.action))
    }

    /// The TOML pairs this change writes. An off limit is written as -1 and
    /// not removed: the key then shows in the settings editor, set to off.
    pub fn pairs(&self) -> Result<Vec<(String, String)>, String> {
        let mut kv = Vec::new();
        if let Some(r) = self.max_ratio {
            let r = if r.is_finite() && r >= 0.0 { r } else { -1.0 };
            // `{:?}` keeps the decimal point: TOML reads `2` as an integer.
            kv.push(("max_ratio".to_string(), format!("{r:?}")));
        }
        if let Some(m) = self.max_seeding_time {
            kv.push(("max_seeding_time".to_string(), m.max(-1).to_string()));
        }
        if let Some(m) = self.max_inactive_seeding_time {
            kv.push(("max_inactive_seeding_time".to_string(), m.max(-1).to_string()));
        }
        if let Some(a) = &self.share_limit_action {
            let a = Action::parse(a).ok_or_else(|| {
                format!("share_limit_action must be stop, remove or remove_with_files, not {a:?}")
            })?;
            kv.push(("share_limit_action".to_string(), format!("\"{}\"", a.as_str())));
        }
        Ok(kv)
    }
}

/// Write an engine's limits where the engine reads them: `[race]` /
/// `[hoard]`, or the `session` of its `[[engine]]` block, created when the
/// file has no such key yet (every file from before 4.4). The worker reads
/// the config on every pass, so nothing else is needed for it to apply.
pub fn write_engine_limits(state: &AppState, engine_id: &str, change: &EngineChange) -> Result<(), String> {
    let kv = change.pairs()?;
    if kv.is_empty() {
        return Ok(());
    }
    let ok = crate::api::edit_config(state, |doc| {
        if engine_id == "race" || engine_id == "hoard" {
            return crate::tomledit::set_toml_table(doc, engine_id, &kv);
        }
        let mut out = doc.to_string();
        for (k, v) in &kv {
            out = crate::tomledit::set_agent_session_key(&out, engine_id, k, v)
                .ok_or_else(|| format!("no [[engine]] block for {engine_id}"))?;
        }
        Ok(out)
    });
    if ok { Ok(()) } else { Err("the config could not be written".into()) }
}

/// The engine whose limits the qBittorrent API's single `preferences` page
/// speaks for: `race` when there is one, as the rest of that page does,
/// else the first local engine.
pub fn preferences_engine(state: &AppState) -> String {
    let ids: Vec<String> = state.engines.engines().iter().map(|e| e.id.clone()).collect();
    if ids.iter().any(|i| i == "race") {
        return "race".into();
    }
    ids.into_iter().next().unwrap_or_else(|| "race".into())
}

/// The share-limit keys of qBittorrent's `app/preferences`.
pub fn qbit_preferences(e: &EngineShareLimits) -> serde_json::Value {
    serde_json::json!({
        "max_ratio_enabled": e.ratio >= 0.0,
        "max_ratio": crate::row::num_json(e.ratio),
        "max_seeding_time_enabled": e.seeding_minutes >= 0,
        "max_seeding_time": e.seeding_minutes,
        "max_inactive_seeding_time_enabled": e.inactive_minutes >= 0,
        "max_inactive_seeding_time": e.inactive_minutes,
        "max_ratio_act": e.action.qbit_code(),
    })
}

/// What a `setPreferences` document asks of the share limits, read the way
/// qBittorrent reads it: a value is taken only with its `*_enabled` flag
/// (`max_ratio` alone changes nothing), and a flag set to false switches the
/// limit off whatever value comes with it.
pub fn change_from_qbit_preferences(doc: &serde_json::Value) -> EngineChange {
    let num = |k: &str| doc.get(k).and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()));
    let flag = |k: &str| {
        doc.get(k).and_then(|v| v.as_bool().or_else(|| match v.as_str()? {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        }))
    };
    let mut c = EngineChange::default();
    if let Some(on) = flag("max_ratio_enabled") {
        c.max_ratio = Some(if on { num("max_ratio").unwrap_or(-1.0) } else { -1.0 });
    }
    if let Some(on) = flag("max_seeding_time_enabled") {
        c.max_seeding_time = Some(if on { num("max_seeding_time").map_or(-1, |v| v as i64) } else { -1 });
    }
    if let Some(on) = flag("max_inactive_seeding_time_enabled") {
        c.max_inactive_seeding_time =
            Some(if on { num("max_inactive_seeding_time").map_or(-1, |v| v as i64) } else { -1 });
    }
    if let Some(code) = num("max_ratio_act") {
        match Action::from_qbit(code as i64) {
            Some(a) => c.share_limit_action = Some(a.as_str().to_string()),
            None => tracing::warn!(code, "setPreferences: max_ratio_act not supported here, the action is unchanged"),
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(toml_src: &str) -> crate::config::Session {
        toml::from_str(toml_src).expect("a session parses")
    }

    /// ⭐ Nothing configured is nothing limited, including the `Default`
    /// session a missing `[race]` table is built from -- a ratio limit of 0
    /// read off a zeroed struct would be reached by every torrent at once.
    #[test]
    fn an_engine_without_the_keys_limits_nothing() {
        assert!(!EngineShareLimits::from_session(&session("")).any());
        assert!(!EngineShareLimits::from_session(&crate::config::Session::default()).any());
        assert!(!EngineShareLimits::from_session(&crate::config::Session::with_defaults()).any());
        let off = EngineShareLimits::from_session(&session(
            "max_ratio = -1.0\nmax_seeding_time = -1\nmax_inactive_seeding_time = -1\n",
        ));
        assert!(!off.any());
        assert_eq!(off.action, Action::Stop);
    }

    #[test]
    fn an_integer_ratio_in_the_file_is_read() {
        let e = EngineShareLimits::from_session(&session("max_ratio = 2\nshare_limit_action = \"remove\"\n"));
        assert_eq!(e.ratio, 2.0);
        assert_eq!(e.action, Action::Remove);
    }

    #[test]
    fn a_torrent_follows_its_engine_unless_it_says_otherwise() {
        let engine = EngineShareLimits { ratio: 2.0, seeding_minutes: 60, inactive_minutes: -1, action: Action::Stop };
        let follow = TorrentShareLimits::default();
        assert_eq!(effective(&follow, &engine), Effective { ratio: 2.0, seeding_minutes: 60, inactive_minutes: -1 });
        let own = TorrentShareLimits { ratio: -1.0, seeding_minutes: 10, inactive_minutes: -2 };
        assert_eq!(effective(&own, &engine), Effective { ratio: -1.0, seeding_minutes: 10, inactive_minutes: -1 });
    }

    #[test]
    fn any_one_limit_is_enough() {
        let eff = Effective { ratio: 1.0, seeding_minutes: 60, inactive_minutes: 30 };
        assert!(matches!(reached(&eff, || 1.0, 0, None), Some(Reached::Ratio { .. })));
        assert!(matches!(reached(&eff, || 0.5, 3600, None), Some(Reached::SeedingTime { .. })));
        assert!(matches!(reached(&eff, || 0.5, 60, Some(1800)), Some(Reached::Inactive { .. })));
        assert_eq!(reached(&eff, || 0.5, 60, Some(60)), None);
        // A limit that is off is never reached, even at 0.
        let off = Effective { ratio: -1.0, seeding_minutes: -1, inactive_minutes: -1 };
        assert_eq!(reached(&off, || 99.0, i64::MAX / 120, Some(i64::MAX / 120)), None);
    }

    #[test]
    fn set_share_limits_values_are_normalized() {
        assert_eq!(normalize_ratio(-2.0), -2.0);
        assert_eq!(normalize_ratio(-1.0), -1.0);
        assert_eq!(normalize_ratio(-0.5), -1.0);
        assert_eq!(normalize_ratio(1.5), 1.5);
        assert_eq!(normalize_minutes(-7), -2);
        assert_eq!(normalize_minutes(-1), -1);
        assert_eq!(normalize_minutes(90), 90);
    }

    /// qBittorrent's table: 0 stop, 1 remove, 2 super seeding (not here),
    /// 3 remove with the content.
    #[test]
    fn max_ratio_act_follows_qbittorrents_numbering() {
        assert_eq!(Action::Stop.qbit_code(), 0);
        assert_eq!(Action::Remove.qbit_code(), 1);
        assert_eq!(Action::RemoveWithFiles.qbit_code(), 3);
        assert_eq!(Action::from_qbit(2), None);
        assert_eq!(Action::from_qbit(3), Some(Action::RemoveWithFiles));
    }

    #[test]
    fn set_preferences_takes_a_value_only_with_its_flag() {
        let c = change_from_qbit_preferences(&serde_json::json!({"max_ratio": 3}));
        assert!(c.is_empty(), "a bare max_ratio changes nothing, as in qBittorrent");
        let c = change_from_qbit_preferences(&serde_json::json!({
            "max_ratio_enabled": true, "max_ratio": 1.5,
            "max_seeding_time_enabled": false, "max_seeding_time": 60,
            "max_ratio_act": 1,
        }));
        assert_eq!(c.max_ratio, Some(1.5));
        assert_eq!(c.max_seeding_time, Some(-1), "disabled is off whatever the value");
        assert_eq!(c.share_limit_action.as_deref(), Some("remove"));
        assert_eq!(c.pairs().unwrap()[0], ("max_ratio".to_string(), "1.5".to_string()));
        let two = EngineChange { max_ratio: Some(2.0), ..Default::default() };
        assert_eq!(two.pairs().unwrap()[0].1, "2.0", "written as a float");
    }
}

#[cfg(test)]
mod worker_tests {
    use crate::api::testing::{state_from, TestState};
    use crate::workers::{share_limits_pass, ShareLimitBook};
    use axum::http::StatusCode;
    use std::sync::atomic::Ordering;
    use tower::ServiceExt;
    use typhon_engine::torrent::meta::{TorrentState, TorrentStatus};

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    const NOW: i64 = 1_700_000_000;
    const SIZE: u64 = 16_384;

    /// A single-file torrent, unique per name (its piece hash carries it).
    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi{SIZE}e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        for (i, b) in name.bytes().enumerate().take(20) {
            piece[i] = b;
        }
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn state(tag: &str, race: &str, min_seed: Option<&str>) -> TestState {
        let declared = min_seed
            .map(|h| format!("\n[announce_min_seed_hours]\n\"tracker.example\" = \"{h}\"\n"))
            .unwrap_or_default();
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n\n[race]\nlisten_port = 16171\n{race}\n{declared}"))
    }

    /// A seed on the race engine with its files on disk, `ratio` x its size
    /// uploaded, `seeded_secs` of seeding behind it.
    fn seed(s: &TestState, name: &str, ratio: u64, seeded_secs: i64) -> (String, std::sync::Arc<TorrentState>, std::path::PathBuf) {
        let dir = s.dir.join("data");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, vec![0u8; SIZE as usize]).unwrap();
        let (hash, _) = crate::api::add_torrent_bytes(
            &s.state, &torrent_bytes(name), "", dir.to_str().unwrap(), "", false, true, "race",
        )
        .expect("added");
        let (_, t) = crate::api::find_torrent(&s.state, &hash).expect("held");
        {
            let mut live = t.live_trackers.write();
            live.clear();
            live.push(vec!["https://tracker.example/announce".to_string()]);
        }
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        t.is_paused.store(false, Ordering::Relaxed);
        t.total_uploaded.store(ratio * SIZE, Ordering::Relaxed);
        t.seed_secs.store(seeded_secs, Ordering::Relaxed);
        t.seed_since.store(0, Ordering::Relaxed);
        (hash, t, file)
    }

    fn pass(s: &TestState, book: &mut ShareLimitBook, now: i64) -> crate::workers::ShareLimitOutcome {
        let mgr = s.engines.get("race").unwrap().manager.clone();
        share_limits_pass(&s.state, &mgr, &s.cfg(), "race", now, book)
    }

    fn user_paused(s: &TestState, hash: &str) -> bool {
        s.store.read().unwrap().paused_hashes("race").unwrap().iter().any(|h| h == hash)
    }

    /// ⭐ The engine's ratio limit stops a seed past it, and only that one;
    /// the stop is the operator's kind (the store's intent), so nothing
    /// restarts it and a qBit client reads it as stoppedUP.
    #[test]
    fn the_worker_stops_a_seed_at_its_ratio() {
        let s = state("sl-ratio", "max_ratio = 2.0", Some("0"));
        let (over, t_over, _) = seed(&s, "over", 3, 0);
        let (under, t_under, _) = seed(&s, "under", 1, 0);
        let out = pass(&s, &mut ShareLimitBook::default(), NOW);
        assert_eq!((out.stopped, out.removed), (1, 0));
        assert!(t_over.is_paused.load(Ordering::Relaxed) && user_paused(&s, &over));
        assert!(!t_under.is_paused.load(Ordering::Relaxed) && !user_paused(&s, &under));
        // Already stopped by it: the next pass leaves it alone.
        assert_eq!(pass(&s, &mut ShareLimitBook::default(), NOW).stopped, 0);
    }

    #[test]
    fn the_worker_stops_a_seed_at_its_seeding_time() {
        let s = state("sl-time", "max_seeding_time = 60", Some("0"));
        let (_, long, _) = seed(&s, "long", 0, 3600);
        let (_, short, _) = seed(&s, "short", 0, 3599);
        assert_eq!(pass(&s, &mut ShareLimitBook::default(), NOW).stopped, 1);
        assert!(long.is_paused.load(Ordering::Relaxed));
        assert!(!short.is_paused.load(Ordering::Relaxed));
    }

    /// Inactivity counts from the first pass that saw the torrent, and an
    /// upload resets it.
    #[test]
    fn the_worker_stops_a_seed_that_uploaded_nothing_for_its_inactive_time() {
        let s = state("sl-idle", "max_inactive_seeding_time = 30", Some("0"));
        let (_, idle, _) = seed(&s, "idle", 0, 0);
        let (_, busy, _) = seed(&s, "busy", 0, 0);
        let mut book = ShareLimitBook::default();
        assert_eq!(pass(&s, &mut book, NOW).stopped, 0, "the clock starts now, not at some unmeasured past");
        busy.total_uploaded.fetch_add(1000, Ordering::Relaxed);
        assert_eq!(pass(&s, &mut book, NOW + 29 * 60).stopped, 0);
        assert_eq!(pass(&s, &mut book, NOW + 30 * 60).stopped, 1);
        assert!(idle.is_paused.load(Ordering::Relaxed));
        assert!(!busy.is_paused.load(Ordering::Relaxed), "its upload 29 minutes in reset its clock");
    }

    /// ⭐⭐ A seed still owing its tracker seeding time is never touched, by
    /// any action, and neither is one on a tracker with no declaration --
    /// the race drain's rule. Declaring the hours is what opts it in.
    #[test]
    fn a_seed_under_its_trackers_obligation_is_never_removed() {
        let s = state("sl-owed", "max_ratio = 1.0\nshare_limit_action = \"remove_with_files\"", Some("72"));
        let (hash, t, file) = seed(&s, "owed", 5, 3600);
        let out = pass(&s, &mut ShareLimitBook::default(), NOW);
        assert_eq!((out.removed, out.stopped, out.held), (0, 0, 1));
        assert!(crate::api::find_torrent(&s.state, &hash).is_some() && file.exists());
        assert!(!t.is_paused.load(Ordering::Relaxed), "not even stopped");

        let s = state("sl-undeclared", "max_ratio = 1.0\nshare_limit_action = \"remove\"", None);
        let (hash, t, _) = seed(&s, "undeclared", 5, 10_000_000);
        let out = pass(&s, &mut ShareLimitBook::default(), NOW);
        assert_eq!((out.removed, out.stopped, out.held), (0, 0, 1));
        assert!(crate::api::find_torrent(&s.state, &hash).is_some());
        assert!(!t.is_paused.load(Ordering::Relaxed));

        // Served: 72 h seeded, now it goes.
        let s = state("sl-served", "max_ratio = 1.0\nshare_limit_action = \"remove\"", Some("72"));
        let (hash, _, file) = seed(&s, "served", 5, 72 * 3600);
        assert_eq!(pass(&s, &mut ShareLimitBook::default(), NOW).removed, 1);
        assert!(crate::api::find_torrent(&s.state, &hash).is_none());
        assert!(file.exists(), "`remove` keeps the files");
    }

    #[test]
    fn remove_with_files_deletes_the_torrent_and_its_data() {
        let s = state("sl-rmfiles", "max_ratio = 1.0\nshare_limit_action = \"remove_with_files\"", Some("0"));
        let (hash, _, file) = seed(&s, "gone", 2, 0);
        assert!(file.exists());
        assert_eq!(pass(&s, &mut ShareLimitBook::default(), NOW).removed, 1);
        assert!(crate::api::find_torrent(&s.state, &hash).is_none(), "out of the engine");
        assert!(s.store.read().unwrap().sessions_of(&hash).is_empty(), "out of the store");
        assert!(!file.exists(), "and its file is gone");
    }

    /// Downloading, or stopped by the operator: neither is the worker's.
    #[test]
    fn a_downloading_or_user_stopped_torrent_is_ignored() {
        let s = state("sl-dl", "max_ratio = 0.0\nshare_limit_action = \"remove\"", Some("0"));
        let (dl, t_dl, _) = seed(&s, "downloading", 5, 0);
        t_dl.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let (stopped, _, _) = seed(&s, "stopped", 5, 0);
        s.store.lock().unwrap().set_paused(&stopped, "race", true).unwrap();
        let out = pass(&s, &mut ShareLimitBook::default(), NOW);
        assert_eq!((out.removed, out.stopped), (0, 0));
        assert!(crate::api::find_torrent(&s.state, &dl).is_some());
        assert!(crate::api::find_torrent(&s.state, &stopped).is_some());
    }

    /// A torrent's own limit acts with the engine's off -- the sparse-table
    /// path -- and -1 exempts a torrent from the engine's.
    #[test]
    fn a_torrents_own_limit_overrides_its_engines() {
        let s = state("sl-own", "max_ratio = 1.0", Some("0"));
        let (exempt, t_exempt, _) = seed(&s, "exempt", 5, 0);
        super::set_torrent_limits(&s.state, &[exempt], Some(-1.0), None, None).unwrap();
        assert_eq!(pass(&s, &mut ShareLimitBook::default(), NOW).stopped, 0);
        assert!(!t_exempt.is_paused.load(Ordering::Relaxed));

        let s = state("sl-own-only", "", Some("0"));
        let (own, t_own, _) = seed(&s, "own", 5, 0);
        let (_, t_other, _) = seed(&s, "other", 5, 0);
        super::set_torrent_limits(&s.state, &[own], Some(4.0), None, None).unwrap();
        assert_eq!(pass(&s, &mut ShareLimitBook::default(), NOW).stopped, 1);
        assert!(t_own.is_paused.load(Ordering::Relaxed));
        assert!(!t_other.is_paused.load(Ordering::Relaxed));
    }

    /// ⭐⭐ The upgrade guarantee: a config without any of the new keys
    /// limits nothing, reports nothing enabled, and every torrent follows its
    /// engine (-2) -- so Sonarr's "Remove Completed" sees no torrent stop.
    #[tokio::test]
    async fn the_defaults_change_nothing() {
        let s = state("sl-defaults", "", Some("0"));
        let (hash, t, _) = seed(&s, "huge", 1000, 100_000_000);
        let out = pass(&s, &mut ShareLimitBook::default(), NOW);
        assert_eq!(out, Default::default());
        assert!(!t.is_paused.load(Ordering::Relaxed));

        let (_, prefs) = call(&s, "GET", "/api/v2/app/preferences", "").await;
        assert_eq!(prefs["max_ratio_enabled"], false);
        assert_eq!(prefs["max_seeding_time_enabled"], false);
        assert_eq!(prefs["max_inactive_seeding_time_enabled"], false);
        assert_eq!(prefs["max_ratio_act"], 0);
        assert_eq!(prefs["queueing_enabled"], false);
        let (_, info) = call(&s, "POST", "/api/v2/torrents/info", "").await;
        let row = &info.as_array().unwrap().iter().find(|r| r["hash"] == hash.as_str()).unwrap().clone();
        assert_eq!(row["ratio_limit"], -2);
        assert_eq!(row["seeding_time_limit"], -2);
        assert_eq!(row["inactive_seeding_time_limit"], -2);
        assert_eq!(row["max_ratio"], -1);
        assert_eq!(row["max_seeding_time"], -1);
        assert_eq!(row["state"], "stalledUP", "seeding, not stopped");
    }

    async fn call(s: &TestState, method: &str, uri: &str, form: &str) -> (StatusCode, serde_json::Value) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("X-API-Key", KEY)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(form.to_string()))
            .unwrap();
        let resp = crate::api::router(s.state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// ⭐ `setShareLimits` over POST, hashes in the body, then in the query
    /// with `all`: stored, and read back by `info` and `properties`; a GET
    /// changes nothing, and the two required fields are required.
    #[tokio::test]
    async fn set_share_limits_over_post_reaches_info_and_properties() {
        let s = state("sl-shim", "max_seeding_time = 600", Some("0"));
        let (a, _, _) = seed(&s, "a", 0, 0);
        let (b, _, _) = seed(&s, "b", 0, 0);
        let (st, _) = call(&s, "POST", "/api/v2/torrents/setShareLimits",
            &format!("hashes={a}&ratioLimit=1.5&seedingTimeLimit=-2&inactiveSeedingTimeLimit=30")).await;
        assert_eq!(st, StatusCode::OK);
        let own = s.store.read().unwrap().share_limits_of(&a);
        assert_eq!((own.ratio, own.seeding_minutes, own.inactive_minutes), (1.5, -2, 30));

        let (_, info) = call(&s, "POST", "/api/v2/torrents/info", &format!("hashes={a}")).await;
        let row = &info[0];
        assert_eq!(row["ratio_limit"], 1.5);
        assert_eq!(row["max_ratio"], 1.5);
        assert_eq!(row["seeding_time_limit"], -2);
        assert_eq!(row["max_seeding_time"], 600, "followed from the engine");
        assert_eq!(row["max_inactive_seeding_time"], 30);
        let (_, props) = call(&s, "POST", "/api/v2/torrents/properties", &format!("hash={a}")).await;
        assert_eq!(props["ratio_limit"], 1.5);
        assert_eq!(props["max_seeding_time"], 600);

        // `all`, in the query of a POST; the inactive limit absent = -2.
        let (st, _) = call(&s, "POST", "/api/v2/torrents/setShareLimits?hashes=all&ratioLimit=-1&seedingTimeLimit=90", "").await;
        assert_eq!(st, StatusCode::OK);
        for h in [&a, &b] {
            let own = s.store.read().unwrap().share_limits_of(h);
            assert_eq!((own.ratio, own.seeding_minutes, own.inactive_minutes), (-1.0, 90, -2));
        }
        let (st, _) = call(&s, "GET", &format!("/api/v2/torrents/setShareLimits?hashes={a}&ratioLimit=9&seedingTimeLimit=9"), "").await;
        assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);
        let (st, _) = call(&s, "POST", "/api/v2/torrents/setShareLimits", &format!("hashes={a}&ratioLimit=2")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "seedingTimeLimit is required, as in qBittorrent");
        // Back to all -2: the row goes, the table stays sparse.
        call(&s, "POST", "/api/v2/torrents/setShareLimits", &format!("hashes={a}|{b}&ratioLimit=-2&seedingTimeLimit=-2")).await;
        assert!(s.store.read().unwrap().share_limits_all().unwrap().is_empty());
    }

    /// `setPreferences` writes the engine keys, every local engine, read the
    /// qBittorrent way; `preferences` reads them back.
    #[tokio::test]
    async fn set_preferences_writes_the_share_limits_of_every_engine() {
        let s = state("sl-prefs", "", Some("0"));
        let json = r#"{"max_ratio_enabled":true,"max_ratio":2,"max_seeding_time_enabled":true,"max_seeding_time":1440,"max_ratio_act":3}"#;
        let body = format!("json={}", json.replace('{', "%7B").replace('}', "%7D").replace('"', "%22").replace(':', "%3A").replace(',', "%2C"));
        let (st, _) = call(&s, "POST", "/api/v2/app/setPreferences", &body).await;
        assert_eq!(st, StatusCode::OK);
        for id in ["race", "hoard"] {
            let e = super::engine_limits(&s.state, id);
            assert_eq!((e.ratio, e.seeding_minutes, e.action), (2.0, 1440, super::Action::RemoveWithFiles), "{id}");
        }
        let (_, prefs) = call(&s, "GET", "/api/v2/app/preferences", "").await;
        assert_eq!(prefs["max_ratio_enabled"], true);
        assert_eq!(prefs["max_ratio"], 2);
        assert_eq!(prefs["max_seeding_time"], 1440);
        assert_eq!(prefs["max_ratio_act"], 3);
        // The file still loads, and nothing in it is called dead.
        let raw = std::fs::read_to_string(&s.config_path).unwrap();
        assert!(crate::deadkeys::config_warnings(&raw).is_empty(), "{raw}");
    }

    /// The native routes: per torrent and per engine, read back.
    #[tokio::test]
    async fn the_native_routes_set_and_read_back() {
        let s = state("sl-native", "", Some("0"));
        let (a, _, _) = seed(&s, "a", 0, 0);
        let req = |m: &str, uri: String, body: &str| {
            axum::http::Request::builder().method(m).uri(uri).header("X-API-Key", KEY)
                .header("content-type", "application/json").body(axum::body::Body::from(body.to_string())).unwrap()
        };
        let resp = crate::api::router(s.state.clone())
            .oneshot(req("POST", format!("/api/torrents/{a}/share-limits"), r#"{"ratio_limit": 3, "seeding_time_limit": -1}"#))
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = crate::api::testing::body_json(resp).await;
        assert_eq!(v["copies"][0]["ratio_limit"], 3);
        assert_eq!(v["copies"][0]["seeding_time_limit"], -1);
        assert_eq!(v["copies"][0]["inactive_seeding_time_limit"], -2);
        let resp = crate::api::router(s.state.clone())
            .oneshot(req("POST", "/api/engines/hoard/share-limits".into(), r#"{"max_ratio": 1.25, "share_limit_action": "remove"}"#))
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = crate::api::testing::body_json(resp).await;
        assert_eq!(v["max_ratio"], 1.25);
        assert_eq!(v["share_limit_action"], "remove");
        assert_eq!(super::engine_limits(&s.state, "race").ratio, -1.0, "the other engine is untouched");
        let resp = crate::api::router(s.state.clone())
            .oneshot(req("POST", "/api/engines/hoard/share-limits".into(), r#"{"share_limit_action": "delete"}"#))
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Sonarr and Radarr send their indexer's seed goal with the add.
    #[tokio::test]
    async fn ratio_and_seeding_time_at_add_land_on_the_torrent() {
        let s = state("sl-add", "", Some("0"));
        let boundary = "XyZ";
        let mut body = Vec::new();
        for (k, v) in [("savepath", s.dir.join("data").to_str().unwrap().to_string()), ("skip_checking", "true".into()),
                       ("ratioLimit", "1.5".into()), ("seedingTimeLimit", "4320".into())] {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
        }
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"torrents\"; filename=\"x.torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n").as_bytes());
        body.extend_from_slice(&torrent_bytes("added"));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = axum::http::Request::builder().method("POST").uri("/api/v2/torrents/add").header("X-API-Key", KEY)
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(axum::body::Body::from(body)).unwrap();
        let resp = crate::api::router(s.state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let all = s.store.read().unwrap().share_limits_all().unwrap();
        assert_eq!(all.len(), 1);
        let own = all.values().next().unwrap();
        assert_eq!((own.ratio, own.seeding_minutes, own.inactive_minutes), (1.5, 4320, -2));
    }
}
