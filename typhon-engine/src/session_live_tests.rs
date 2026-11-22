use super::*;
use crate::bench_support as bench;
use crate::torrent::meta::TorrentState;
use std::sync::atomic::Ordering;

pub(super) fn manager(tag: &str, n: usize) -> (Arc<TorrentManager>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("hydranos-live-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(TorrentManager::new(
        root.join("data").to_string_lossy().into(),
        root.join("resume").to_string_lossy().into(),
        Arc::new(DiskManager::new(16)),
    ));
    let mut data = b"d4:infod6:lengthi16384e4:name4:test12:piece lengthi16384e6:pieces20:".to_vec();
    data.extend_from_slice(&[42u8; 20]);
    data.extend_from_slice(b"ee");
    let base = crate::torrent::metainfo::parse_torrent_bytes(&data).unwrap();
    for i in 0..n {
        let mut meta = base.clone();
        meta.info_hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
        let state = Arc::new(TorrentState::new(meta, root.join("data"), true));
        state.is_paused.store(false, Ordering::Relaxed);
        state.status.store(3, Ordering::Relaxed);
        manager.insert_prebuilt(state);
    }
    (manager, root)
}

#[test]
fn emitter_primes_cold_rows_without_sending_them_and_releases_idle_cache() {
    let (manager, root) = manager("idle", 3);
    let mut emitter = StatsEmitter::default();
    assert!(emitter.collect(&manager).is_empty());
    assert_eq!(emitter.last.capacity(), 0);
    let listener = manager.bus().subscribe();
    assert!(emitter.collect(&manager).is_empty());
    assert_eq!(emitter.last.len(), 3);
    assert!(emitter.collect(&manager).is_empty());
    manager.all()[0]
        .total_uploaded
        .fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        emitter.collect(&manager).len(),
        1,
        "a previously cold row still emits its first change"
    );
    drop(listener);
    assert!(emitter.collect(&manager).is_empty());
    assert_eq!(emitter.last.capacity(), 0);
    let _listener = manager.bus().subscribe();
    assert!(emitter.collect(&manager).is_empty());
    assert_eq!(emitter.last.len(), 3);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}

/// Opt-in synthetic measurement. Run with --ignored --nocapture --test-threads=1.
#[test]
#[ignore]
fn benchmark_live_emitter() {
    bench::start();
    let n = bench::count();
    let (manager, root) = manager("bench", n);
    let mut emitter = StatsEmitter::default();
    let rss_before = bench::resident_bytes();
    for listeners in [0, 1, 10] {
        let mut receivers: Vec<_> = (0..listeners).map(|_| manager.bus().subscribe()).collect();
        let first_cpu = bench::cpu_ms();
        let first_at = std::time::Instant::now();
        let first = emitter.collect(&manager);
        let first_ms = first_at.elapsed().as_secs_f64() * 1000.0;
        let first_cpu_ms = bench::cpu_ms() - first_cpu;
        assert!(
            first.is_empty(),
            "initial cold rows are loaded through REST"
        );
        let snapshot_bytes = if first.is_empty() {
            0
        } else {
            serde_json::to_vec(&first).unwrap().len()
        };
        drop(first);
        let cpu = bench::cpu_ms();
        let at = std::time::Instant::now();
        for _ in 0..20 {
            assert!(emitter.collect(&manager).is_empty());
        }
        let steady_cpu = (bench::cpu_ms() - cpu) / 20.0;
        let steady_ms = at.elapsed().as_secs_f64() * 1000.0 / 20.0;
        let hot: Vec<_> = manager.all().into_iter().take(n.min(100)).collect();
        let cpu = bench::cpu_ms();
        let at = std::time::Instant::now();
        let mut bytes = 0;
        for _ in 0..10 {
            for t in &hot {
                t.total_downloaded.fetch_add(16384, Ordering::Relaxed);
            }
            let changed = emitter.collect(&manager);
            if listeners > 0 {
                assert_eq!(changed.len(), hot.len());
            }
            if !changed.is_empty() {
                manager
                    .bus()
                    .publish(crate::rpc::events::Event::StatsSnapshot { torrents: changed });
                for receiver in &mut receivers {
                    bytes = serde_json::to_vec(&receiver.try_recv().unwrap())
                        .unwrap()
                        .len();
                }
            }
        }
        bench::emit(
            "emitter",
            serde_json::json!({"catalogue":n, "listeners":listeners,
            "prime_ms": first_ms, "prime_cpu_ms":first_cpu_ms, "initial_rows_json_bytes":snapshot_bytes, "steady_tick_ms":steady_ms, "steady_cpu_ms":steady_cpu,
            "changing_tick_ms":at.elapsed().as_secs_f64()*1000.0/10.0,
            "changing_cpu_ms":(bench::cpu_ms()-cpu)/10.0, "delta_rows":if listeners>0 {hot.len()} else {0},
            "per_listener_json_bytes":bytes, "aggregate_json_bytes": bytes*listeners, "stamp_entries":emitter.last.len(),
            "stamp_capacity":emitter.last.capacity(), "resident_bytes":bench::resident_bytes(),
            "resident_before_listeners":rss_before}),
        );
        drop(receivers);
        emitter.collect(&manager);
    }
    let cpu = bench::cpu_ms();
    let at = std::time::Instant::now();
    for _ in 0..20 {
        manager.update_rates();
    }
    bench::emit(
        "rate_tick",
        serde_json::json!({"catalogue":n,
        "tick_ms":at.elapsed().as_secs_f64()*1000.0/20.0, "cpu_ms":(bench::cpu_ms()-cpu)/20.0}),
    );
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}
