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
        state.set_paused(false, Ordering::Relaxed);
        state.set_status(3, Ordering::Relaxed);
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

fn drain_rows(
    receiver: &mut tokio::sync::broadcast::Receiver<crate::rpc::events::Event>,
) -> Vec<crate::rpc::events::TorrentStatsMini> {
    let mut rows = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok(crate::rpc::events::Event::StatsSnapshot { torrents }) => {
                assert!(torrents.len() <= 512);
                rows.extend(torrents);
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(e) => panic!("unexpected stream error: {e}"),
        }
    }
    rows
}

#[test]
fn dirty_rows_cover_cold_changes_peers_checks_and_reconnects() {
    let (manager, root) = manager("dirty-live", 3);
    let row = manager.all()[0].clone();
    let mut receiver = manager.bus().subscribe();
    manager.update_rates();
    assert!(
        drain_rows(&mut receiver).is_empty(),
        "no initial cold burst"
    );
    row.total_uploaded.fetch_add(10, Ordering::Relaxed);
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver)[0].total_uploaded, 10);
    row.restore_lifetime(5, 3);
    manager.update_rates();
    let rows = drain_rows(&mut receiver);
    assert_eq!((rows[0].total_uploaded, rows[0].total_downloaded), (5, 3));
    row.set_paused(true, Ordering::Relaxed);
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver)[0].state, "paused");
    row.set_paused(false, Ordering::Relaxed);
    manager.update_rates();
    drain_rows(&mut receiver);
    let stats = Arc::new(crate::torrent::meta::PeerStats::new(
        "127.0.0.1:12345".parse().unwrap(),
        [0; 20],
        String::new(),
        false,
        false,
    ));
    let guard = crate::torrent::meta::PeerGuard::new(row.clone(), stats);
    guard.mark_interested(true);
    manager.update_rates();
    let rows = drain_rows(&mut receiver);
    assert_eq!((rows[0].peers_connected, rows[0].peers_interested), (1, 1));
    drop(guard);
    manager.update_rates();
    let rows = drain_rows(&mut receiver);
    assert_eq!((rows[0].peers_connected, rows[0].peers_interested), (0, 0));
    let proof = row.hold_for_proof();
    let waiting = proof.waiting();
    manager.update_rates();
    let rows = drain_rows(&mut receiver);
    assert_eq!(rows[0].state, "checking_files");
    drop(waiting);
    drop(proof);
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver)[0].state, "seeding");
    row.begin_recheck_progress();
    row.set_rechecking(true, Ordering::SeqCst);
    let waiting = row.wait_for_check();
    manager.update_rates();
    assert!(drain_rows(&mut receiver)[0].check_queued);
    drop(waiting);
    row.record_recheck_read();
    manager.update_rates();
    let rows = drain_rows(&mut receiver);
    assert_eq!(rows[0].checking_progress, Some(1.0));
    assert!(!rows[0].check_queued);
    row.end_recheck_progress();
    row.set_rechecking(false, Ordering::SeqCst);
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver)[0].state, "seeding");
    drop(receiver);
    row.set_paused(true, Ordering::Relaxed);
    manager.update_rates();
    let mut receiver = manager.bus().subscribe();
    manager.update_rates();
    assert!(
        drain_rows(&mut receiver).is_empty(),
        "REST owns reconnect hydration"
    );
    row.set_paused(false, Ordering::Relaxed);
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver).len(), 1);
    row.set_status(
        crate::torrent::meta::TorrentStatus::Downloading as u8,
        Ordering::Relaxed,
    );
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver)[0].state, "downloading");
    manager.update_rates();
    assert!(
        drain_rows(&mut receiver).is_empty(),
        "idle downloads do not generate redundant snapshots"
    );
    manager.set_seed_only(&row.info_hash, true).unwrap();
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver)[0].state, "seed_only");
    row.mark_error("test data unavailable");
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver)[0].state, "error");
    drop(receiver);
    drop(row);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn dirty_bulk_changes_are_batched_and_racing_marks_survive() {
    let (manager, root) = manager("dirty-bulk", 1100);
    let mut receiver = manager.bus().subscribe();
    manager.update_rates();
    assert!(drain_rows(&mut receiver).is_empty());
    for row in manager.all() {
        row.set_paused(true, Ordering::Relaxed);
    }
    manager.update_rates();
    assert_eq!(drain_rows(&mut receiver).len(), 1100);
    let row = manager.all()[0].clone();
    assert!(!row.take_live_dirty(true, false));
    row.mark_live_dirty();
    assert!(row.take_live_dirty(true, false));
    // A write after the clear must remain pending for the next delivery.
    row.mark_live_dirty();
    assert!(row.take_live_dirty(true, false));
    assert!(!row.take_live_dirty(true, false));
    drop(receiver);
    drop(row);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn dirty_marks_preserve_the_final_update_during_concurrent_drains() {
    let (manager, root) = manager("dirty-race", 1);
    let row = manager.all()[0].clone();
    row.take_live_dirty(false, false);
    let writer_row = row.clone();
    let writer = std::thread::spawn(move || {
        for _ in 0..20_000 {
            writer_row.total_uploaded.fetch_add(1, Ordering::Relaxed);
            writer_row.mark_live_dirty();
        }
    });
    let mut last = 0;
    while !writer.is_finished() {
        if row.take_live_dirty(true, false) {
            last = row.total_uploaded.load(Ordering::Relaxed);
        }
        std::thread::yield_now();
    }
    writer.join().unwrap();
    if row.take_live_dirty(true, false) {
        last = row.total_uploaded.load(Ordering::Relaxed);
    }
    assert_eq!(
        last, 20_000,
        "the final mark must be observed or remain pending"
    );
    drop(row);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn dirty_emitter_delivers_the_shared_rate_pass() {
    let (manager, root) = manager("dirty-task", 1);
    manager.update_rates();
    let row = manager.all()[0].clone();
    let mut receiver = manager.bus().subscribe();
    row.set_paused(true, Ordering::Relaxed);
    let task = spawn_stats_emitter(manager.clone());
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::rpc::events::Event::StatsSnapshot { torrents } = event else {
        panic!("expected shared rate-pass snapshot")
    };
    assert_eq!(torrents.len(), 1);
    assert_eq!(torrents[0].state, "paused");
    task.abort();
    let _ = task.await;
    drop(receiver);
    drop(row);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[ignore]
fn benchmark_dirty_rate_pass() {
    bench::start();
    let n = bench::count();
    for mode in ["fingerprint", "dirty_flags"] {
        let (manager, root) = manager(&format!("rate-{mode}"), n);
        let mut receiver = manager.bus().subscribe();
        let mut emitter = StatsEmitter::default();
        let rss_before = bench::resident_bytes();
        let tick = |emitter: &mut StatsEmitter| {
            if mode == "fingerprint" {
                manager.update_rates_observed(|_| {});
                let torrents = emitter.collect(&manager);
                if !torrents.is_empty() {
                    manager
                        .bus()
                        .publish(crate::rpc::events::Event::StatsSnapshot { torrents });
                }
            } else {
                manager.update_rates();
            }
        };
        tick(&mut emitter);
        assert!(drain_rows(&mut receiver).is_empty());
        let rss_primed = bench::resident_bytes();
        let hot: Vec<_> = manager.all().into_iter().take(100.min(n)).collect();
        for changing in [false, true] {
            let mut elapsed = 0.0;
            let mut cpu = 0.0;
            for _ in 0..20 {
                if changing {
                    for row in &hot {
                        row.total_uploaded.fetch_add(16384, Ordering::Relaxed);
                    }
                }
                let before_cpu = bench::cpu_ms();
                let at = std::time::Instant::now();
                tick(&mut emitter);
                elapsed += at.elapsed().as_secs_f64() * 1000.0;
                cpu += bench::cpu_ms() - before_cpu;
                assert_eq!(
                    drain_rows(&mut receiver).len(),
                    if changing { hot.len() } else { 0 }
                );
            }
            bench::emit(
                "dirty_rate",
                serde_json::json!({
                    "catalogue":n,"mode":mode,"changing":changing,
                    "tick_ms":elapsed/20.0,"cpu_ms":cpu/20.0,
                    "rss_before":rss_before,"rss_primed":rss_primed,
                    "fingerprint_entries":emitter.last.len(),
                    "fingerprint_capacity":emitter.last.capacity(),
                }),
            );
        }
        drop(hot);
        drop(receiver);
        drop(manager);
        std::fs::remove_dir_all(root).ok();
    }
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
