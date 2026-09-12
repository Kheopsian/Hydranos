//! The torrent row the API publishes.
//!
//! This is a faithful port of `engine.TorrentStats` and the two functions that
//! fill it, `LtStatusToTorrentStats` and `DeriveState`. Three things about it
//! are contractual and none of them are guessable:
//!
//!   * **Field order.** Go's encoding/json writes a struct in DECLARATION
//!     order, not sorted. serde does the same, so the fields below are kept in
//!     exactly the order the Go struct declares them. (A map is different: Go
//!     sorts map keys, which is why /api/settings, built from a map, matches
//!     while being alphabetical.)
//!   * **omitempty.** In Go it drops the field when the value is the zero
//!     value. An absent field and a field set to "" are different bytes, and
//!     clients written against 3.x see the difference.
//!   * **Where each value comes from.** Several fields are the STORE's value
//!     with the engine's as a fallback -- added_time, completed_time,
//!     save_path. Taking the engine's first would have been the obvious
//!     reading and it is wrong: the bench caught completed_time doing exactly
//!     that on 486 real torrents.

use serde_json::Value;

pub const STATE_STOPPED: &str = "stopped";
pub const STATE_QUEUED: &str = "queued";

/// What the store knows about a torrent, which the engine does not.
///
/// Category, tags and the rest are the front's business: the engine moves
/// bytes and has no opinion about how a user filed the result.
#[derive(Debug, Clone, Default)]
pub struct StoreFacts {
    pub category: String,
    pub save_path: String,
    pub added_time: i64,
    pub completed_time: i64,
    pub seeding_time: i64,
    pub tags: Vec<String>,
    pub user_paused: bool,
    pub content_folder: Option<bool>,
}

/// Go emits a float64 the way encoding/json does: an integral value prints
/// without a decimal point. serde_json would print 1.0 where Go prints 1, which
/// changes the bytes of every row.
pub fn num_json(value: f64) -> Value {
    num(value)
}

fn num(value: f64) -> Value {
    if value.fract() == 0.0 && value.is_finite() && value.abs() < 9.0e15 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

/// The state the API reports, from the engine's raw state and the user's intent.
///
/// The engine says "paused" for anything halted, and cannot tell a scheduler
/// hold from a user pressing stop. The intent flag is authoritative because the
/// engine may still be reporting the state it had a tick before the stop
/// landed.
/// `derive_state` without the allocation.
///
/// Every value it can return is one of a fixed set, so the list pass -- which
/// runs this 300k times per request -- has no reason to build a String each
/// time. The owned version stays for the row builders, which need one anyway.
pub fn derive_state_static(raw: &'static str, user_stopped: bool) -> &'static str {
    match raw {
        "paused" | STATE_STOPPED | STATE_QUEUED | "" => {
            if user_stopped { STATE_STOPPED } else { STATE_QUEUED }
        }
        _ if user_stopped => STATE_STOPPED,
        other => other,
    }
}

pub fn derive_state(raw: &str, user_stopped: bool) -> String {
    match raw {
        "paused" | STATE_STOPPED | STATE_QUEUED | "" => {
            if user_stopped { STATE_STOPPED } else { STATE_QUEUED }.to_string()
        }
        _ if user_stopped => STATE_STOPPED.to_string(),
        other => other.to_string(),
    }
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn i(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn f(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn b(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Project one engine torrent, plus what the store knows, into an API row.
///
/// The shape is a MAP, not a struct, and that is not a detail: encoding/json
/// sorts map keys, so the wire order is alphabetical, and a map has no
/// omitempty -- every key below is always present, `tags` and `content_folder`
/// included, as JSON null when they have no value. Building this from a struct
/// with omitempty produced a row missing eight keys and ordered differently,
/// which the bench caught against 486 real torrents.
///
/// The key set was read off a live 3.x answer rather than inferred from the Go
/// types: the engine publishes list_seeds, list_peers, total_done, is_announced
/// and active_time, and the front deliberately does not forward any of them.
pub fn build(engine: &Value, facts: &StoreFacts, agent: &str) -> Value {
    let raw_state = s(engine, "state");
    let state = derive_state(&raw_state, facts.user_paused);

    // A seeding torrent is complete by definition; the engine's own progress
    // can sit a hair under 1.0 and the UI would render 99% forever.
    let progress = if raw_state == "seeding" { 1.0 } else { f(engine, "progress") };

    let total_download = i(engine, "total_download");
    let total_upload = i(engine, "total_upload");
    let ratio = if total_download > 0 {
        total_upload as f64 / total_download as f64
    } else {
        0.0
    };

    let engine_save_path = s(engine, "save_path");
    let save_path = if facts.save_path.is_empty() {
        engine_save_path.clone()
    } else {
        facts.save_path.clone()
    };

    let added_time = if facts.added_time > 0 { facts.added_time } else { i(engine, "added_time") };
    let completed_time = if facts.completed_time > 0 {
        facts.completed_time
    } else {
        i(engine, "completed_time")
    };

    let mut row = serde_json::Map::new();
    row.insert("added_time".into(), added_time.into());
    row.insert("agent".into(), agent.into());
    row.insert("category".into(), facts.category.clone().into());
    row.insert("completed_time".into(), completed_time.into());
    row.insert(
        "content_folder".into(),
        match facts.content_folder {
            Some(v) => Value::Bool(v),
            None => Value::Null,
        },
    );
    row.insert("download_rate".into(), i(engine, "download_rate").into());
    row.insert("engine_save_path".into(), engine_save_path.into());
    row.insert("info_hash".into(), s(engine, "info_hash").into());
    row.insert("injected_peers".into(), 0.into());
    row.insert("injection_hit".into(), false.into());
    row.insert("multi_file".into(), b(engine, "multi_file").into());
    row.insert("name".into(), s(engine, "name").into());
    row.insert("num_peers".into(), i(engine, "num_peers").into());
    // Not the engine's num_seeds: 3.x publishes the tracker's seed count here.
    row.insert("num_seeds".into(), i(engine, "list_seeds").into());
    row.insert("progress".into(), num(progress));
    row.insert("ratio".into(), num(ratio));
    row.insert("save_path".into(), save_path.into());
    // The engine's live counter wins over the store's copy. The store is
    // written once an hour -- enough for a rule that asks about 48 hours, far
    // too coarse for a panel a person is looking at, which would sit on a
    // stale number for up to an hour and show 0 for the first one.
    //
    // The store value is the fallback, for a torrent the engines no longer
    // hold: there the last synced figure is all there is.
    let live_seed = i(engine, "seeding_time");
    row.insert(
        "seeding_time".into(),
        if live_seed > 0 { live_seed.into() } else { facts.seeding_time.into() },
    );
    row.insert("state".into(), state.into());
    row.insert("swarm_leechers".into(), i(engine, "list_peers").into());
    row.insert("swarm_seeds".into(), i(engine, "list_seeds").into());
    row.insert(
        "tags".into(),
        if facts.tags.is_empty() {
            Value::Null
        } else {
            Value::Array(facts.tags.iter().map(|t| Value::String(t.clone())).collect())
        },
    );
    row.insert("torrent_error".into(), (raw_state == "error").into());
    row.insert("torrent_error_msg".into(), s(engine, "error_msg").into());
    row.insert("total_download".into(), total_download.into());
    row.insert("total_size".into(), i(engine, "total_size").into());
    row.insert("total_upload".into(), total_upload.into());
    row.insert("tracker_error".into(), b(engine, "tracker_error").into());
    row.insert("tracker_error_msg".into(), s(engine, "tracker_error_msg").into());
    row.insert("tracker_host".into(), s(engine, "tracker_host").into());
    row.insert("upload_rate".into(), i(engine, "upload_rate").into());
    row.insert("uploader".into(), "".into());
    row.insert("user_paused".into(), facts.user_paused.into());
    Value::Object(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn halted_states_depend_on_intent() {
        assert_eq!(derive_state("paused", false), "queued");
        assert_eq!(derive_state("paused", true), "stopped");
        assert_eq!(derive_state("", false), "queued");
        // Intent wins even when the engine still claims to be seeding.
        assert_eq!(derive_state("seeding", true), "stopped");
        assert_eq!(derive_state("seeding", false), "seeding");
    }

    #[test]
    fn seeding_is_reported_complete() {
        let row = build(&json!({"state": "seeding", "progress": 0.9997}), &StoreFacts::default(), "");
        assert_eq!(row["progress"], json!(1));
    }

    // The bench caught this on 486 real torrents: reading completed_time from
    // the engine rather than the store is the obvious implementation and the
    // wrong one.
    #[test]
    fn the_store_wins_over_the_engine_for_dates_and_path() {
        let engine = json!({
            "added_time": 111, "completed_time": 222, "save_path": "/engine",
        });
        let facts = StoreFacts {
            added_time: 999, completed_time: 888, save_path: "/store".into(),
            ..Default::default()
        };
        let row = build(&engine, &facts, "");
        assert_eq!(row["added_time"], 999);
        assert_eq!(row["completed_time"], 888);
        assert_eq!(row["save_path"], "/store");
        assert_eq!(row["engine_save_path"], "/engine", "the engine path is still published");

        // and the engine is the fallback when the store says nothing
        let row = build(&engine, &StoreFacts::default(), "");
        assert_eq!(row["added_time"], 111);
        assert_eq!(row["completed_time"], 222);
        assert_eq!(row["save_path"], "/engine");
    }

    #[test]
    fn ratio_is_zero_rather_than_infinite_without_downloads() {
        let row = build(&json!({"total_upload": 500, "total_download": 0}), &StoreFacts::default(), "");
        assert_eq!(row["ratio"], json!(0));
        let row = build(&json!({"total_upload": 500, "total_download": 250}), &StoreFacts::default(), "");
        assert_eq!(row["ratio"], json!(2));
    }

    // The exact key set of a 3.x row, read off a live answer. Both halves
    // matter: a missing key breaks a client, and an extra one (the engine
    // publishes several the front hides) leaks internals the UI never showed.
    #[test]
    fn the_key_set_matches_a_live_3x_row() {
        let row = build(&json!({"info_hash": "abc", "name": "x"}), &StoreFacts::default(), "local-race");
        let keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, vec![
            "added_time", "agent", "category", "completed_time", "content_folder",
            "download_rate", "engine_save_path", "info_hash", "injected_peers",
            "injection_hit", "multi_file", "name", "num_peers", "num_seeds",
            "progress", "ratio", "save_path", "seeding_time", "state",
            "swarm_leechers", "swarm_seeds", "tags", "torrent_error",
            "torrent_error_msg", "total_download", "total_size", "total_upload",
            "tracker_error", "tracker_error_msg", "tracker_host", "upload_rate",
            "uploader", "user_paused",
        ]);
    }

    // Absent values are null, not omitted: a 3.x row always carries the key.
    #[test]
    fn unset_tags_and_content_folder_are_null() {
        let row = build(&json!({}), &StoreFacts::default(), "");
        assert_eq!(row["tags"], Value::Null);
        assert_eq!(row["content_folder"], Value::Null);
    }

    // encoding/json prints an integral float64 without a decimal point.
    #[test]
    fn integral_floats_print_as_integers() {
        assert_eq!(serde_json::to_string(&num(1.0)).unwrap(), "1");
        assert_eq!(serde_json::to_string(&num(17.5)).unwrap(), "17.5");
    }
}
