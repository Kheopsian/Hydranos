//! Opt-in, socket-free synthetic measurements of the actual page routes.
use super::testing::*;
use super::*;
use crate::bench_support as bench;
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn benchmark_live_pages() {
    bench::start();
    let n = bench::count();
    assert!(n >= 400);
    let s = state("live-page-bench");
    let manager = &s.state.engines.get("hoard").unwrap().manager;
    let mut bytes =
        b"d4:infod6:lengthi16384e4:name4:test12:piece lengthi16384e6:pieces20:".to_vec();
    bytes.extend_from_slice(&[42u8; 20]);
    bytes.extend_from_slice(b"ee");
    let base = typhon_engine::torrent::metainfo::parse_torrent_bytes(&bytes).unwrap();
    for i in 0..n {
        let mut meta = base.clone();
        meta.info_hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
        let t = Arc::new(typhon_engine::torrent::meta::TorrentState::new(
            meta,
            std::env::temp_dir(),
            true,
        ));
        t.is_paused.store(false, Ordering::Relaxed);
        t.status.store(
            if i < 100 {
                2
            } else if i < 200 {
                1
            } else if i < 300 {
                4
            } else {
                3
            },
            Ordering::Relaxed,
        );
        manager.insert_prebuilt(t);
    }
    manager.update_rates();
    let tick_cpu = bench::cpu_ms();
    let tick_at = std::time::Instant::now();
    for _ in 0..100 {
        manager.update_rates();
    }
    bench::emit(
        "index",
        serde_json::json!({"catalogue":n, "hash_storage_bytes":manager.live_index_bytes(),
            "rate_tick_ms":tick_at.elapsed().as_secs_f64()*1000.0/100.0, "rate_tick_cpu_ms":(bench::cpu_ms()-tick_cpu)/100.0,
        "downloading":manager.live_candidates("downloading").unwrap().len(),
        "checking":manager.live_candidates("checking_files").unwrap().len(),
        "errors":manager.live_candidates("__error__").unwrap().len()}),
    );
    // Warm the store's shared slim facts before comparing selection paths.
    engine_page_value(&s.state, "hoard", "state=downloading&facets=1").await;
    for filter in ["downloading", "checking_files", "__error__"] {
        for facets in [true, false] {
            let query = format!("state={filter}&facets={}&limit=500", u8::from(facets));
            let repeats = if facets { 10 } else { 1000 };
            let cpu = bench::cpu_ms();
            let at = std::time::Instant::now();
            let mut row_count = 0;
            for _ in 0..repeats {
                let page = engine_page_value(&s.state, "hoard", &query).await;
                row_count = page["rows"].as_array().unwrap().len();
                assert_eq!(row_count, 100);
            }
            bench::emit(
                "page",
                serde_json::json!({"catalogue":n, "filter":filter,
                "full_scan":facets, "rows":row_count, "mean_ms":at.elapsed().as_secs_f64()*1000.0/repeats as f64,
                "cpu_ms":(bench::cpu_ms()-cpu)/repeats as f64}),
            );
        }
    }
    for cached in [false, true] {
        s.state.page_refreshes.clear().await;
        let before = s.state.page_refreshes.computations.load(Ordering::Relaxed);
        let query = if cached {
            "state=downloading&facets=1&live=1"
        } else {
            "state=downloading&facets=1"
        };
        let cpu = bench::cpu_ms();
        let at = std::time::Instant::now();
        let responses =
            futures::future::join_all((0..10).map(|_| get_engine_page(&s.state, "hoard", query)))
                .await;
        for response in responses {
            assert_eq!(body_json(response).await["filtered"], 100);
        }
        bench::emit(
            "ten_clients",
            serde_json::json!({"catalogue":n, "cached":cached,
            "wall_ms":at.elapsed().as_secs_f64()*1000.0, "cpu_ms":bench::cpu_ms()-cpu,
            "cache_computations":s.state.page_refreshes.computations.load(Ordering::Relaxed)-before,
            "resident_bytes":bench::resident_bytes()}),
        );
    }
}
