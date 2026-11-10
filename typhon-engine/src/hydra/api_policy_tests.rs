//! The two routes that decide when the race drain may delete: what a tracker
//! requires a torrent to be seeded (0 releases it, nothing declared protects
//! it) and a volume's own marks. What they refuse, what they save, and what
//! the drain will then read back.

use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

async fn declare(s: &TestState, body: &str) -> (StatusCode, serde_json::Value) {
    let resp = set_announce_min_seed(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    let code = resp.status();
    (code, body_json(resp).await)
}

fn declared(s: &TestState, host: &str) -> Option<String> {
    s.state.cfg().announce_min_seed_hours.get(host).cloned()
}

fn on_disk(s: &TestState) -> Config {
    toml::from_str(&std::fs::read_to_string(&s.state.config_path).unwrap()).unwrap()
}

/// ⭐ What a tracker requires is saved as written, to the file and to the
/// running config: `0` is a value (it releases the torrent), clearing removes
/// it (the torrent is protected again), and the two are not the same.
#[tokio::test]
async fn a_tracker_s_seeding_time_is_saved_zero_is_kept_and_clearing_removes_it() {
    let s = st("policy-minseed");

    let (code, v) = declare(&s, r#"{"host":" tracker.example ","hours":48}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        serde_json::json!({
            "host": "tracker.example", "min_seed_hours": 48, "cleared": false, "persisted": true
        })
    );
    assert_eq!(declared(&s, "tracker.example").as_deref(), Some("48"));
    assert_eq!(
        on_disk(&s)
            .announce_min_seed_hours
            .get("tracker.example")
            .map(String::as_str),
        Some("48"),
        "and it survives a restart"
    );

    let (_, v) = declare(&s, r#"{"host":"tracker.example","hours":0}"#).await;
    assert_eq!(v["min_seed_hours"], 0);
    assert_eq!(
        declared(&s, "tracker.example").as_deref(),
        Some("0"),
        "0 is stored: it is the one way to release a torrent"
    );

    let (_, v) = declare(&s, r#"{"host":"other.example","hours":24}"#).await;
    assert_eq!(v["persisted"], true);
    let (_, v) = declare(&s, r#"{"host":"tracker.example","hours":7,"clear":true}"#).await;
    assert_eq!(
        v,
        serde_json::json!({
            "host": "tracker.example", "min_seed_hours": null, "cleared": true, "persisted": true
        })
    );
    assert_eq!(declared(&s, "tracker.example"), None, "nothing declared");
    assert_eq!(
        declared(&s, "other.example").as_deref(),
        Some("24"),
        "another tracker's is not touched"
    );
    assert_eq!(on_disk(&s).announce_min_seed_hours.len(), 1);
}

/// A body that is no request, a host that is nothing and hours below zero are
/// refused with their reason, and nothing is written.
#[tokio::test]
async fn a_seeding_time_that_makes_no_sense_is_refused_and_writes_nothing() {
    let s = st("policy-minseed-bad");
    let before = std::fs::read_to_string(&s.state.config_path).unwrap();
    for (body, error) in [
        ("not json", "invalid body"),
        (r#"{"hours":5}"#, "invalid body"),
        (r#"{"host":"   ","hours":5}"#, "host is required"),
        (
            r#"{"host":"tracker.example","hours":-1}"#,
            "hours cannot be negative",
        ),
    ] {
        let (code, v) = declare(&s, body).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(v["error"], error, "{body}");
    }
    assert_eq!(
        std::fs::read_to_string(&s.state.config_path).unwrap(),
        before
    );
    assert!(s.state.cfg().announce_min_seed_hours.is_empty());
    let resp = set_announce_min_seed(
        State(s.state.clone()),
        RawQuery(None),
        axum::http::HeaderMap::new(),
        r#"{"host":"tracker.example","hours":1}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(s.state.cfg().announce_min_seed_hours.is_empty());
}

async fn volume(s: &TestState, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let resp = set_volume_policy(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        Json(serde_json::from_value(body).unwrap()),
    )
    .await;
    let code = resp.status();
    (code, body_json(resp).await)
}

fn policy(s: &TestState, id: &str) -> crate::volumes::Policy {
    crate::volumes::policy_for(&s.state, id, &s.state.cfg().race_drain)
}

/// ⭐ A volume's marks are saved as a whole, kept when only one of them
/// changes, and given back to the global ones when the volume inherits.
#[tokio::test]
async fn a_volumes_marks_are_saved_kept_and_given_back() {
    let s = st("policy-volume");
    let inherited = policy(&s, "/mnt/ssd");
    assert!(inherited.inherited);

    let (code, v) = volume(
        &s,
        serde_json::json!({
            "volume": "/mnt/ssd", "enabled": true, "high_watermark": 90, "low_watermark": 80
        }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        (
            v["enabled"].clone(),
            v["high_watermark"].clone(),
            v["low_watermark"].clone(),
            v["inherited"].clone()
        ),
        (true.into(), 90.into(), 80.into(), false.into())
    );
    let p = policy(&s, "/mnt/ssd");
    assert_eq!(
        (p.enabled, p.high, p.low, p.inherited),
        (true, 90, 80, false)
    );

    // Only the switch: the marks stay.
    let (_, v) = volume(
        &s,
        serde_json::json!({"volume": "/mnt/ssd", "enabled": false}),
    )
    .await;
    assert_eq!(
        (v["high_watermark"].clone(), v["low_watermark"].clone()),
        (90.into(), 80.into())
    );
    let p = policy(&s, "/mnt/ssd");
    assert_eq!((p.enabled, p.high, p.low), (false, 90, 80));
    // Only one mark: the other stays.
    let (_, v) = volume(
        &s,
        serde_json::json!({"volume": "/mnt/ssd", "low_watermark": 70}),
    )
    .await;
    assert_eq!(
        (v["high_watermark"].clone(), v["low_watermark"].clone()),
        (90.into(), 70.into())
    );
    // Another volume is not touched.
    assert!(policy(&s, "/mnt/other").inherited);

    let (_, v) = volume(
        &s,
        serde_json::json!({"volume": "/mnt/ssd", "inherit": true}),
    )
    .await;
    assert_eq!(v["status"], "ok");
    assert_eq!(v["inherited"], true);
    assert_eq!(policy(&s, "/mnt/ssd").inherited, true);
    assert_eq!(policy(&s, "/mnt/ssd").high, inherited.high);
}

/// What would drain forever, or never, or nothing sensible, is refused with
/// its reason and the volume's marks stay as they were.
#[tokio::test]
async fn marks_that_cannot_work_are_refused_and_change_nothing() {
    let s = st("policy-volume-bad");
    volume(
        &s,
        serde_json::json!({"volume": "/v", "enabled": true, "high_watermark": 90, "low_watermark": 80}),
    )
    .await;
    for (body, error) in [
        (serde_json::json!({}), "invalid"),
        (serde_json::json!({"volume": ""}), "volume is required"),
        (
            serde_json::json!({"volume": "/v", "high_watermark": 80, "low_watermark": 80}),
            "down to must be below start",
        ),
        (
            serde_json::json!({"volume": "/v", "high_watermark": 70}),
            "down to must be below start",
        ),
        (
            serde_json::json!({"volume": "/v", "high_watermark": 101, "low_watermark": 50}),
            "watermarks are percentages",
        ),
        (
            serde_json::json!({"volume": "/v", "high_watermark": 50, "low_watermark": 0}),
            "watermarks are percentages",
        ),
        (
            serde_json::json!({"volume": "/v", "quota_gb": -1.0}),
            "quota_gb is a size in GB, 0 to remove it",
        ),
    ] {
        if error == "invalid" {
            // No volume at all is the extractor's refusal, not the route's.
            assert!(serde_json::from_value::<VolumePolicyBody>(body).is_err());
            continue;
        }
        let (code, v) = volume(&s, body.clone()).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}: {v}");
        assert_eq!(v["error"], error, "{body}");
    }
    let p = policy(&s, "/v");
    assert_eq!((p.enabled, p.high, p.low), (true, 90, 80), "unchanged");
    assert_eq!(crate::volumes::quota_for(&s.state, "/v"), None);
}

/// A quota and a TRIM setting are not drain rules: saving either does not
/// freeze today's defaults onto the volume as an override.
#[tokio::test]
async fn a_quota_or_a_trim_setting_leaves_the_marks_inherited() {
    let s = st("policy-volume-quota");

    let (_, v) = volume(
        &s,
        serde_json::json!({"volume": "/q", "trim_after_drain": true}),
    )
    .await;
    assert_eq!(
        v,
        serde_json::json!({"status": "ok", "trim_after_drain": true})
    );
    assert!(crate::volumes::trim_enabled(&s.state, "/q"));
    assert!(!crate::volumes::trim_enabled(&s.state, "/not-q"));

    let (_, v) = volume(&s, serde_json::json!({"volume": "/q", "quota_gb": 2.5})).await;
    assert_eq!(
        v,
        serde_json::json!({"status": "ok", "quota": 2_500_000_000u64, "trim_after_drain": true})
    );
    assert_eq!(
        crate::volumes::quota_for(&s.state, "/q"),
        Some(2_500_000_000)
    );
    assert!(policy(&s, "/q").inherited, "still the global marks");

    let (_, v) = volume(&s, serde_json::json!({"volume": "/q", "quota_gb": 0})).await;
    assert_eq!(v["quota"], serde_json::Value::Null);
    assert_eq!(
        crate::volumes::quota_for(&s.state, "/q"),
        None,
        "0 drops it"
    );

    // A quota together with a mark is both: the mark is saved too.
    let (_, v) = volume(
        &s,
        serde_json::json!({"volume": "/q", "quota_gb": 1, "enabled": true,
                           "high_watermark": 95, "low_watermark": 85}),
    )
    .await;
    assert_eq!(v["quota"], 1_000_000_000u64);
    assert_eq!(v["high_watermark"], 95);
    let p = policy(&s, "/q");
    assert_eq!((p.high, p.low, p.inherited), (95, 85, false));
    // And turning TRIM off is as much a setting as turning it on.
    let (_, v) = volume(
        &s,
        serde_json::json!({"volume": "/q", "trim_after_drain": false}),
    )
    .await;
    assert_eq!(v["trim_after_drain"], false);
    assert!(!crate::volumes::trim_enabled(&s.state, "/q"));
}
