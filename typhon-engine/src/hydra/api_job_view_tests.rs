use super::job_view;

/// A graduation carries its torrent's resume record in its params, tracker
/// URLs included. The job listing shows what the job does, not that.
#[test]
fn the_job_listing_leaves_out_the_record_a_graduation_carries() {
    let job = crate::store::Job {
        id: "j1".into(),
        kind: "graduate".into(),
        state: "running".into(),
        info_hash: "ab".repeat(20),
        params: serde_json::json!({
            "to_engine": "hoard",
            "record": {"trackers": [["https://tracker.example/announce?k=secret"]]},
        })
        .to_string(),
        progress_bytes: 0,
        total_bytes: 1,
        error: String::new(),
        created_at: 0,
        updated_at: 0,
    };
    let view = job_view(&job);
    assert!(view["params"].get("record").is_none());
    assert_eq!(
        view["params"]["to_engine"], "hoard",
        "the rest is still shown"
    );
}
