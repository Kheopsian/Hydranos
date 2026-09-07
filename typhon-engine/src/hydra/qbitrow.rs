//! The torrent row as qBittorrent clients expect it.
//!
//! This is the single most consequential conversion in the shim. Sonarr,
//! Radarr, cross-seed and autobrr all poll /api/v2/torrents/info and act on
//! what it says, and none of them reports a mismatch: a wrong state makes an
//! import wait forever, a wrong content_path makes cross-seed link into a
//! directory that does not exist.
//!
//! Ported from hydraToQbitTorrent + hydraStateToQbit.

use serde_json::Value;

/// qBittorrent's "infinite" ETA.
const ETA_INFINITE: i64 = 8_640_000;

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

/// Translate Hydra's state into qBittorrent's vocabulary.
///
/// The distinction between "stopped" and "queued" is the one that matters to a
/// client: stopped means a human said so and the client should leave it alone;
/// queued means a scheduler is holding it and it will move on its own.
pub fn state_to_qbit(
    state: &str,
    progress: f64,
    upload_rate: i64,
    download_rate: i64,
    user_paused: bool,
) -> &'static str {
    let complete = progress >= 1.0;

    if user_paused {
        return if complete { "stoppedUP" } else { "stoppedDL" };
    }
    match state {
        "downloading" => {
            if download_rate > 0 { "downloading" } else { "stalledDL" }
        }
        "seeding" => {
            if upload_rate > 0 { "uploading" } else { "stalledUP" }
        }
        "checking" | "checking_files" => "checkingDL",
        "stopped" => {
            if complete { "stoppedUP" } else { "stoppedDL" }
        }
        "queued" | "paused" => {
            if complete { "queuedUP" } else { "queuedDL" }
        }
        "error" => "error",
        "moving" => "moving",
        "allocating" => "allocating",
        _ => {
            if complete { "stalledUP" } else { "stalledDL" }
        }
    }
}

/// Convert one native row into the qBittorrent shape.
///
/// `engine_name` is the fallback category: a torrent with no category of its
/// own is reported under its engine, because *arr refuses to import a torrent
/// whose category it does not recognise.
pub fn build(native: &Value, engine_name: &str, now: i64) -> Value {
    let progress = f(native, "progress");
    let download_rate = i(native, "download_rate");
    let upload_rate = i(native, "upload_rate");
    let state = state_to_qbit(
        &s(native, "state"),
        progress,
        upload_rate,
        download_rate,
        b(native, "user_paused"),
    );

    let total_size = i(native, "total_size");
    // ⚠⚠ BUG DE 3.x REPRODUIT TEL QUEL.
    //
    // The Go shim reads "total_downloaded" and "total_uploaded"; the native row
    // it is handed carries "total_download" and "total_upload". The keys do not
    // exist, so every qBittorrent client sees uploaded = 0 and ratio = 0 for
    // the whole library -- measured here on 486 real torrents, where the true
    // ratio is 4.2 and the shim reports 0.
    //
    // Reproduced because *arr, cross-seed and autobrr have been reading zeros
    // for as long as this shim has existed, and some of them act on ratio.
    // Publishing the real figure would change their behaviour on the day of an
    // upgrade that is supposed to change nothing. Worth fixing on purpose, in
    // its own release, with a note -- exactly like the tag registry and the
    // pin/unpin asymmetry.
    let mut downloaded = i(native, "total_downloaded");
    let uploaded = i(native, "total_uploaded");

    // A finished torrent reports zero bytes downloaded, because the engine only
    // counts this session. Left alone, *arr sees a complete torrent that never
    // downloaded anything and refuses to import it.
    let mut completed = downloaded;
    let mut amount_left = total_size - downloaded;
    if progress >= 1.0 {
        completed = total_size;
        amount_left = 0;
        if downloaded == 0 {
            downloaded = total_size;
        }
    }

    let ratio = if downloaded > 0 {
        ((uploaded as f64 / downloaded as f64) * 100.0).round() / 100.0
    } else {
        0.0
    };

    let eta = if download_rate > 0 && total_size > 0 {
        if amount_left > 0 { amount_left / download_rate } else { 0 }
    } else {
        ETA_INFINITE
    };

    let added_on = match i(native, "added_time") {
        0 => now,
        value => value,
    };

    let category = match s(native, "category") {
        c if c.is_empty() => engine_name.to_string(),
        c => c,
    };

    // save_path is the ENGINE's directory and name is the torrent's own, so
    // that content_path is the two joined. Deriving the name from the save path
    // instead yields the parent folder for a single-file torrent, and every
    // cross-seed link built from the listing then points at the wrong place.
    let name = s(native, "name");
    let save_path = match s(native, "engine_save_path") {
        p if p.is_empty() => s(native, "save_path"),
        p => p,
    };
    let content_path = if save_path.is_empty() {
        name.clone()
    } else {
        format!("{}/{}", save_path.trim_end_matches('/'), name)
    };

    let tags = native
        .get("tags")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();

    serde_json::json!({
        "hash": s(native, "info_hash"),
        "name": name,
        "state": state,
        "progress": crate::row::num_json(progress),
        "size": total_size,
        "total_size": total_size,
        "dlspeed": download_rate,
        "upspeed": upload_rate,
        "num_seeds": i(native, "num_seeds"),
        "num_leechs": i(native, "num_peers"),
        "num_complete": i(native, "num_seeds"),
        "num_incomplete": i(native, "num_peers"),
        "ratio": crate::row::num_json(ratio),
        "eta": eta,
        "added_on": added_on,
        "completion_on": i(native, "completed_time"),
        "category": category,
        "tags": tags,
        "save_path": save_path,
        "content_path": content_path,
        "downloaded": downloaded,
        "uploaded": uploaded,
        "amount_left": amount_left,
        "completed": completed,
        "seen_complete": 0,
        "priority": 0,
        "seq_dl": false,
        "f_l_piece_prio": false,
        "auto_tmm": false,
        "super_seeding": false,
        "force_start": false,
        "magnet_uri": "",
        "time_active": now - added_on,
        "tracker": s(native, "tracker"),
        "availability": crate::row::num_json(f(native, "availability")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The pair a client acts on: stopped means a human decided, queued means a
    // scheduler is holding it. Confusing them makes an *arr either give up on a
    // torrent or wait forever for one.
    #[test]
    fn stopped_and_queued_are_not_the_same_thing() {
        assert_eq!(state_to_qbit("seeding", 1.0, 0, 0, true), "stoppedUP");
        assert_eq!(state_to_qbit("queued", 1.0, 0, 0, false), "queuedUP");
        assert_eq!(state_to_qbit("queued", 0.5, 0, 0, false), "queuedDL");
        assert_eq!(state_to_qbit("stopped", 0.5, 0, 0, false), "stoppedDL");
    }

    #[test]
    fn activity_decides_between_stalled_and_moving() {
        assert_eq!(state_to_qbit("seeding", 1.0, 0, 0, false), "stalledUP");
        assert_eq!(state_to_qbit("seeding", 1.0, 5000, 0, false), "uploading");
        assert_eq!(state_to_qbit("downloading", 0.5, 0, 0, false), "stalledDL");
        assert_eq!(state_to_qbit("downloading", 0.5, 0, 5000, false), "downloading");
    }

    // Without this fixup a finished torrent claims it downloaded nothing, and
    // *arr refuses to import it.
    #[test]
    fn a_finished_torrent_reports_its_full_size_as_downloaded() {
        let row = build(
            &json!({"progress": 1.0, "total_size": 1000, "total_downloaded": 0, "state": "seeding"}),
            "race", 0,
        );
        assert_eq!(row["downloaded"], 1000);
        assert_eq!(row["completed"], 1000);
        assert_eq!(row["amount_left"], 0);
    }

    #[test]
    fn eta_is_infinite_when_nothing_is_moving() {
        let row = build(&json!({"total_size": 1000, "total_downloaded": 10}), "race", 0);
        assert_eq!(row["eta"], ETA_INFINITE);

        let row = build(
            &json!({"total_size": 1000, "total_downloaded": 100, "download_rate": 90}),
            "race", 0,
        );
        assert_eq!(row["eta"], 10, "900 bytes left at 90 per second");
    }

    // content_path is save_path + name, and cross-seed hard-links from it.
    #[test]
    fn content_path_joins_the_engine_directory_and_the_torrent_name() {
        let row = build(
            &json!({"name": "Release.2160p", "engine_save_path": "/race/torrents",
                    "save_path": "/ignored"}),
            "race", 0,
        );
        assert_eq!(row["content_path"], "/race/torrents/Release.2160p");
        assert_eq!(row["save_path"], "/race/torrents");
    }

    #[test]
    fn an_empty_category_falls_back_to_the_engine_name() {
        let row = build(&json!({"category": ""}), "hoard", 0);
        assert_eq!(row["category"], "hoard");
        let row = build(&json!({"category": "Movies"}), "hoard", 0);
        assert_eq!(row["category"], "Movies");
    }

    #[test]
    fn ratio_is_rounded_to_two_decimals() {
        let row = build(&json!({"total_uploaded": 1000, "total_downloaded": 300}), "race", 0);
        assert_eq!(row["ratio"], json!(3.33));
    }

    // The bug above, pinned so nobody "fixes" it by accident: a native row uses
    // total_upload / total_download, and the shim reads the -ed spellings, so
    // the figures come out zero.
    #[test]
    fn the_shim_reads_the_ed_spelling_and_therefore_reports_zero() {
        let row = build(
            &json!({"total_upload": 999, "total_download": 500, "total_size": 500}),
            "race", 0,
        );
        assert_eq!(row["uploaded"], 0, "3.x reports 0 here; see the comment in build()");
        assert_eq!(row["ratio"], json!(0));
    }
}
